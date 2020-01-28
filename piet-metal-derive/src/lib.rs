//! TODO: explain in detail how this works.
//!
//! A few notes that will be helpful. Structs are encoded differently depending
//! on whether they appear as a variant in an enum; if so, the tag is included.
//! This allows the alignment of the struct to take the tag into account.

extern crate proc_macro;
#[macro_use]
extern crate quote;

use std::collections::HashSet;
use std::fmt::Write;
use std::ops::Deref;

use proc_macro::TokenStream;
use syn::{parse::Parse, parse::ParseStream, parse_macro_input, spanned::Spanned};
use syn::{
    Data, Expr, ExprLit, Fields, FieldsNamed, FieldsUnnamed, GenericArgument, ItemEnum, ItemStruct,
    Lit, PathArguments, TypeArray, TypePath,
};

#[derive(Clone, Copy, PartialEq)]
enum GpuScalar {
    I8,
    I16,
    I32,
    F32,
    U8,
    U16,
    U32,
}

#[derive(Clone)]
enum GpuType {
    Scalar(GpuScalar),
    Vector(GpuScalar, usize),
    /// Used mostly for the body of enum variants.
    InlineStruct(String),
    Ref(Box<GpuType>),
}

struct GpuEnum {
    name: String,
    variants: Vec<(String, Vec<GpuType>)>,
}

enum GpuTypeDef {
    Struct(String, Vec<(String, GpuType)>),
    Enum(GpuEnum),
}

struct GpuModule {
    name: String,
    /// Set of item names that are used as enum variants.
    enum_variants: HashSet<String>,
    defs: Vec<GpuTypeDef>,
}

impl GpuScalar {
    fn metal_typename(self) -> &'static str {
        match self {
            GpuScalar::F32 => "float",
            GpuScalar::I8 => "char",
            GpuScalar::I16 => "short",
            GpuScalar::I32 => "int",
            GpuScalar::U8 => "uchar",
            GpuScalar::U16 => "ushort",
            GpuScalar::U32 => "uint",
        }
    }

    fn hlsl_typename(self) -> &'static str {
        match self {
            GpuScalar::F32 => "float",
            GpuScalar::I32 => "int",
            GpuScalar::U32 => "uint",
            // everything else is stored in a uint (ignoring F64 for now)
            _ => "uint",
        }
    }

    fn size(self) -> usize {
        match self {
            GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => 4,
            GpuScalar::I8 | GpuScalar::U8 => 1,
            GpuScalar::I16 | GpuScalar::U16 => 2,
        }
    }

    fn from_syn(ty: &syn::Type) -> Option<Self> {
        ty_as_single_ident(ty).and_then(|ident| match ident.as_str() {
            "f32" => Some(GpuScalar::F32),
            "i8" => Some(GpuScalar::I8),
            "i16" => Some(GpuScalar::I16),
            "i32" => Some(GpuScalar::I32),
            "u8" => Some(GpuScalar::U8),
            "u16" => Some(GpuScalar::U16),
            "u32" => Some(GpuScalar::U32),
            _ => None,
        })
    }
}

impl std::fmt::Display for GpuScalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuScalar::F32 => write!(f, "F32"),
            GpuScalar::I8 => write!(f, "I8"),
            GpuScalar::I16 => write!(f, "I16"),
            GpuScalar::I32 => write!(f, "I32"),
            GpuScalar::U8 => write!(f, "U8"),
            GpuScalar::U16 => write!(f, "U16"),
            GpuScalar::U32 => write!(f, "U32"),
        }
    }
}

/// If `c = 0`, return `"var_name`, else `"var_name + c"`
fn simplified_add(var_name: &str, c: usize) -> String {
    if c == 0 {
        String::from(var_name)
    } else {
        format!("{} + {}", var_name, c)
    }
}

/// Return number of `uints` required to store `num_bytes` bytes.
fn size_in_uints(num_bytes: usize) -> usize {
    // a `uint` has a size of 4 bytes, (size_in_bytes + 4 - 1) / 4
    (num_bytes + 3) / 4
}

fn generate_hlsl_value_extractor(size_in_bits: u32) -> String {
    if size_in_bits > 31 {
        panic!("nonsensical to generate an extractor for a value with bit size greater than 31");
    }
    let mut extractor: String = String::new();

    let mask_width: usize = 2_usize.pow(size_in_bits) - 1;

    write!(
        extractor,
        "inline uint extract_{}bit_value(uint bit_shift, uint package) {{\n",
        size_in_bits
    )
    .unwrap();
    write!(extractor, "    uint mask = {};\n", mask_width).unwrap();
    write!(
        extractor,
        "{}",
        "    uint result = (package >> bit_shift) & mask;\n\n    return result;\n}\n\n"
    )
    .unwrap();

    extractor
}

/// A `PackedField` stores `StoredField`s
#[derive(Clone)]
struct StoredField {
    name: String,
    ty: GpuType,
    offset: usize,
}

/// A `PackedStruct` has `PackedField`s
#[derive(Clone)]
struct PackedField {
    name: String,
    ty: Option<GpuType>,
    stored_fields: Vec<StoredField>,
    size: usize,
}

/// Possible results of the `pack` method on a `PackedField`.
enum PackResult {
    SuccessAndOpen,
    SuccessAndClosed,
    FailAndClosed,
}

#[derive(Clone)]
struct PackedStruct {
    name: String,
    packed_fields: Vec<PackedField>,
    is_enum_variant: bool,
}

struct SpecifiedStruct {
    name: String,
    fields: Vec<(String, GpuType)>,
    packed_form: PackedStruct,
}

impl StoredField {
    fn generate_hlsl_unpacker(&self, packed_struct_name: &str, packed_field_name: &str) -> String {
        let mut unpacker = String::new();

        if self.ty.is_small() {
            match self.ty {
                GpuType::Scalar(scalar) => {
                    let size_in_bits = 8 * scalar.size();
                    let hlsl_typename: String = match scalar {
                        GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => {
                            panic!("unexpected unpacking of 32 bit value!")
                        }
                        _ => String::from(scalar.hlsl_typename()),
                    };

                    write!(
                        unpacker,
                        "inline uint {}_unpack_{}(uint {}) {{\n    {} result;\n\n",
                        packed_struct_name, self.name, packed_field_name, hlsl_typename,
                    )
                    .unwrap();

                    write!(
                        unpacker,
                        "    result = extract_{}bit_value({}, {});\n",
                        size_in_bits, self.offset, packed_field_name
                    )
                    .unwrap();
                }
                GpuType::Vector(scalar, unpacked_size) => {
                    let scalar_size_in_bits = 8 * scalar.size();
                    let hlsl_typename: String = match scalar {
                        GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => {
                            panic!("unexpected unpacking of 32 bit value!")
                        }
                        _ => String::from(scalar.hlsl_typename()),
                    };

                    let size_in_uints = size_in_uints(&scalar.size() * unpacked_size);
                    write!(
                        unpacker,
                        "inline uint{} {}_unpack_{}(uint{} {}) {{\n    {}{} result;\n\n",
                        unpacked_size,
                        packed_struct_name,
                        self.name,
                        size_in_uints,
                        packed_field_name,
                        hlsl_typename,
                        unpacked_size,
                    )
                    .unwrap();

                    for i in 0..unpacked_size {
                        write!(
                            unpacker,
                            "    result[{}] = extract_{}bit_value({}, {});\n",
                            i,
                            scalar_size_in_bits,
                            32 - (i + 1) * scalar_size_in_bits,
                            packed_field_name
                        )
                        .unwrap();
                    }
                }
                _ => panic!(
                    "only expected small types, got: {}",
                    self.ty.hlsl_typename()
                ),
            }

            write!(unpacker, "{}", "    return result;\n").unwrap();
            write!(unpacker, "{}", "}\n\n").unwrap();
        }

        unpacker
    }
}

impl PackedField {
    fn new() -> PackedField {
        PackedField {
            name: String::new(),
            ty: None,
            size: 0,
            stored_fields: vec![],
        }
    }

    fn pack(
        &mut self,
        module: &GpuModule,
        field_type: &GpuType,
        field_name: &str,
    ) -> Result<PackResult, String> {
        if !self.is_closed() {
            let field_size = field_type.size(module);

            if field_size + self.size > 4 {
                if self.is_empty() {
                    self.stored_fields.push(StoredField {
                        name: String::from(field_name),
                        ty: field_type.clone(),
                        offset: 0,
                    });
                    self.close(module).unwrap();
                    Ok(PackResult::SuccessAndClosed)
                } else {
                    self.close(module).unwrap();
                    Ok(PackResult::FailAndClosed)
                }
            } else {
                self.size += field_size;
                self.stored_fields.push(StoredField {
                    name: String::from(field_name),
                    ty: field_type.clone(),
                    offset: 32 - self.size * 8,
                });
                Ok(PackResult::SuccessAndOpen)
            }
        } else {
            Err("cannot extend closed package".into())
        }
    }

    fn is_empty(&self) -> bool {
        self.stored_fields.len() == 0
    }

    fn is_closed(&self) -> bool {
        self.ty.is_some()
    }

    fn close(&mut self, module: &GpuModule) -> Result<(), String> {
        if !self.is_closed() {
            if self.is_empty() {
                Err("cannot close empty package".into())
            } else {
                let stored_field_names = self
                    .stored_fields
                    .iter()
                    .map(|pf| pf.name.clone())
                    .collect::<Vec<String>>();
                self.name = stored_field_names.join("_");

                self.ty = match self.stored_fields.len() {
                    0 => Err("a packed field must contain at least one stored field"),
                    1 => {
                        let pfty = &self.stored_fields[0].ty;
                        match pfty {
                            GpuType::Scalar(scalar) => match scalar {
                                GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => {
                                    Ok(Some(pfty.clone()))
                                }
                                _ => Ok(Some(GpuType::Scalar(GpuScalar::U32))),
                            },
                            GpuType::Vector(scalar, size) => match scalar {
                                GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => {
                                    Ok(Some(pfty.clone()))
                                }
                                _ => Ok(Some(GpuType::Vector(
                                    GpuScalar::U32,
                                    size_in_uints(scalar.size() * size),
                                ))),
                            },
                            GpuType::InlineStruct(_) => Ok(Some(pfty.clone())),
                            GpuType::Ref(inner) => {
                                if let GpuType::InlineStruct(_) = inner.deref() {
                                    Ok(Some(pfty.clone()))
                                } else {
                                    Ok(Some(GpuType::Scalar(GpuScalar::U32)))
                                }
                            }
                        }
                    }
                    _ => match self.stored_fields.iter().any(|pf| pf.ty.size(module) == 32) {
                        true => Err(
                            "cannot pack multiple types along with at least one 32 bit sized type"
                                .into(),
                        ),
                        false => {
                            let summed_size: usize =
                                self.stored_fields.iter().map(|pf| pf.ty.size(module)).sum();
                            let size_in_uints = (summed_size + 4 - 1) / 4;
                            match size_in_uints {
                                0 => Err("encountered struct of size 0".into()),
                                1 => Ok(Some(GpuType::Scalar(GpuScalar::U32))),
                                2 | 3 | 4 => {
                                    Ok(Some(GpuType::Vector(GpuScalar::U32, size_in_uints)))
                                }
                                _ => Err("packed fields require more than 8 bytes to store".into()),
                            }
                        }
                    },
                }?;
                Ok(())
            }
        } else {
            Err("cannot close closed package".into())
        }
    }

    fn generate_hlsl_reader(&self, current_offset: usize) -> Result<String, String> {
        if let Some(ty) = &self.ty {
            let type_name = ty.hlsl_typename();
            let packed_field_name = &self.name;

            match ty {
                GpuType::Scalar(_) => Ok(format!(
                    "    {} {} = buf.Load({});\n",
                    type_name,
                    packed_field_name,
                    simplified_add("ref", current_offset),
                )),
                GpuType::Vector(scalar, size) => match size {
                    0 => Err("vector of size 0 is not well defined!".into()),
                    1 => Ok(format!(
                        "    {}{} {} = buf.Load({});\n",
                        scalar.hlsl_typename(),
                        size,
                        packed_field_name,
                        simplified_add("ref", current_offset)
                    )),
                    _ => Ok(format!(
                        "    {}{} {} = buf.Load{}({});\n",
                        scalar.hlsl_typename(),
                        size,
                        packed_field_name,
                        size,
                        simplified_add("ref", current_offset)
                    )),
                },
                GpuType::InlineStruct(isn) => Ok(format!(
                    "    {}Packed {} = {}Packed_read(buf, {});\n",
                    isn,
                    packed_field_name,
                    isn,
                    simplified_add("ref", current_offset)
                )),
                GpuType::Ref(inner) => {
                    if let GpuType::InlineStruct(isn) = inner.deref() {
                        Ok(format!(
                            "    {}Ref {} = buf.Load({});\n",
                            isn,
                            packed_field_name,
                            simplified_add("ref", current_offset),
                        ))
                    } else {
                        Ok(format!(
                            "    uint {} = buf.Load({});\n",
                            packed_field_name,
                            simplified_add("ref", current_offset),
                        ))
                    }
                }
            }
        } else {
            Err("cannot generate field reader from an open packed field".into())
        }
    }

    fn generate_hlsl_accessor(
        &self,
        packed_struct_name: &str,
        ref_type: &str,
        reader: &str,
    ) -> Result<String, String> {
        if let Some(ty) = &self.ty {
            let mut field_accessor = String::new();

            match ty {
                GpuType::InlineStruct(_) => {
                    write!(
                        field_accessor,
                        "inline {}Packed {}_{}(ByteAddressBuffer buf, {} ref) {{\n",
                        ty.hlsl_typename(),
                        packed_struct_name,
                        self.name,
                        ref_type,
                    )
                    .unwrap();
                }
                _ => {
                    write!(
                        field_accessor,
                        "inline {} {}_{}(ByteAddressBuffer buf, {} ref) {{\n",
                        ty.hlsl_typename(),
                        packed_struct_name,
                        self.name,
                        ref_type,
                    )
                    .unwrap();
                }
            }
            write!(field_accessor, "{}", reader).unwrap();
            write!(field_accessor, "    return {};\n}}\n\n", self.name).unwrap();

            Ok(field_accessor)
        } else {
            Err("cannot generate field accessor from open packed field".into())
        }
    }

    fn generate_hlsl_unpackers(&self, packed_struct_name: &str) -> String {
        let mut unpackers = String::new();

        for sf in &self.stored_fields {
            write!(
                unpackers,
                "{}",
                sf.generate_hlsl_unpacker(packed_struct_name, &self.name)
            )
            .unwrap();
        }

        unpackers
    }

    fn size(&self, module: &GpuModule) -> Result<usize, String> {
        if let Some(ty) = &self.ty {
            Ok(ty.size(module))
        } else {
            Err("cannot calculate size of open packed field".into())
        }
    }
}

impl PackedStruct {
    fn new(module: &GpuModule, name: &str, fields: &Vec<(String, GpuType)>) -> PackedStruct {
        let mut packed_fields: Vec<PackedField> = Vec::new();

        let mut current_packed_field = PackedField::new();
        for (field_name, ty) in fields {
            match current_packed_field.pack(module, &ty, &field_name).unwrap() {
                PackResult::SuccessAndClosed => {
                    packed_fields.push(current_packed_field.clone());
                    current_packed_field = PackedField::new();
                }
                PackResult::FailAndClosed => {
                    packed_fields.push(current_packed_field.clone());
                    current_packed_field = PackedField::new();
                    current_packed_field.pack(module, &ty, &field_name).unwrap();
                }
                _ => {}
            }
        }

        if !current_packed_field.is_closed() {
            if !current_packed_field.is_empty() {
                current_packed_field.close(module).unwrap();
                packed_fields.push(current_packed_field.clone());
            }
        }

        PackedStruct {
            name: format!("{}Packed", name),
            packed_fields,
            is_enum_variant: module.enum_variants.contains(name),
        }
    }

    fn generate_hlsl_functions(&self, module: &GpuModule) -> String {
        let mut r = String::new();
        let mut field_accessors: Vec<String> = Vec::new();
        let mut unpackers: Vec<String> = Vec::new();

        let ref_type = format!("{}Ref", self.name);

        write!(
            r,
            "inline {} {}_read(ByteAddressBuffer buf, {} ref) {{\n",
            self.name, self.name, ref_type,
        )
        .unwrap();
        write!(r, "    {} result;\n\n", self.name).unwrap();

        let mut current_offset: usize = 0;
        if self.is_enum_variant {
            // account for tag
            current_offset = 4;
        }

        for packed_field in &self.packed_fields {
            let reader: String = packed_field.generate_hlsl_reader(current_offset).unwrap();
            let field_accessor: String = packed_field
                .generate_hlsl_accessor(&self.name, &ref_type, &reader)
                .unwrap();

            field_accessors.push(field_accessor);
            unpackers.push(packed_field.generate_hlsl_unpackers(&self.name));

            write!(r, "{}", reader).unwrap();
            write!(
                r,
                "    result.{} = {};\n\n",
                packed_field.name, packed_field.name
            )
            .unwrap();

            current_offset += packed_field.size(module).unwrap();
        }

        write!(r, "    return result;\n}}\n\n",).unwrap();

        for field_accessor in field_accessors {
            write!(r, "{}", field_accessor).unwrap();
        }

        for unpacker in unpackers {
            write!(r, "{}", unpacker).unwrap();
        }

        r
    }

    fn generate_hlsl_structure_def(&self) -> String {
        let mut r = String::new();

        // The packed struct definition (is missing variable sized arrays)
        write!(r, "struct {} {{\n", self.name).unwrap();
        if self.is_enum_variant {
            write!(r, "    uint tag;\n").unwrap();
        }

        for packed_field in self.packed_fields.iter() {
            match packed_field.ty.as_ref().unwrap() {
                GpuType::InlineStruct(name) => {
                    // a packed struct will only store the packed version of any structs
                    write!(r, "    {}Packed {};\n", name, packed_field.name)
                }
                _ => write!(
                    r,
                    "    {} {};\n",
                    packed_field
                        .ty
                        .as_ref()
                        .expect(&format!("packed field {} has no type", packed_field.name))
                        .hlsl_typename(),
                    packed_field.name
                ),
            }
            .unwrap()
        }
        write!(r, "{}", "};\n\n").unwrap();

        r
    }

    fn to_hlsl(&self, module: &GpuModule) -> String {
        let mut r = String::new();

        write!(r, "{}", self.generate_hlsl_structure_def()).unwrap();
        write!(r, "{}", self.generate_hlsl_functions(module)).unwrap();

        r
    }
}

impl SpecifiedStruct {
    fn new(module: &GpuModule, name: &str, fields: Vec<(String, GpuType)>) -> SpecifiedStruct {
        let packed_form = PackedStruct::new(module, name, &fields);

        SpecifiedStruct {
            name: name.to_string(),
            fields,
            packed_form,
        }
    }

    fn generate_hlsl_structure_def(&self) -> String {
        let mut r = String::new();

        // The packed struct definition (is missing variable sized arrays)
        write!(r, "struct {} {{\n", self.name).unwrap();

        for (field_name, field_type) in self.fields.iter() {
            write!(r, "    {} {};\n", field_type.hlsl_typename(), field_name).unwrap()
        }
        write!(r, "{}", "};\n\n").unwrap();

        r
    }

    fn generate_hlsl_unpacker(&self) -> String {
        let mut r = String::new();

        write!(
            r,
            "inline {} {}_unpack({} packed_form) {{\n",
            self.name, self.packed_form.name, self.packed_form.name,
        )
        .unwrap();

        write!(r, "    {} result;\n\n", self.name).unwrap();
        for (field_name, field_type) in self.fields.iter() {
            let packed_field = self
                .packed_form
                .packed_fields
                .iter()
                .find(|&pf| {
                    pf.stored_fields
                        .iter()
                        .find(|&sf| sf.name == field_name.as_str())
                        .is_some()
                })
                .expect(&format!(
                    "no packed field stores {} in {}Packed",
                    field_name, self.name
                ));
            match field_type {
                GpuType::InlineStruct(name) => {
                    write!(
                        r,
                        "    result.{} = {}Packed_unpack(packed_form.{});\n",
                        field_name, name, packed_field.name
                    )
                    .unwrap();
                }
                _ => {
                    write!(
                        r,
                        "    result.{} = {}_unpack_{}(packed_form.{});\n",
                        field_name, self.packed_form.name, field_name, packed_field.name
                    )
                    .unwrap();
                }
            }
        }
        write!(r, "{}", "\n    return result;\n}\n\n").unwrap();
        r
    }

    fn to_hlsl(&self) -> String {
        let mut r = String::new();

        write!(r, "{}", self.generate_hlsl_structure_def()).unwrap();
        write!(r, "{}", self.generate_hlsl_unpacker()).unwrap();

        r
    }
}

impl GpuType {
    fn metal_typename(&self) -> String {
        match self {
            GpuType::Scalar(scalar) => scalar.metal_typename().into(),
            GpuType::Vector(scalar, size) => format!("{}{}", scalar.metal_typename(), size),
            GpuType::InlineStruct(name) => format!("{}Packed", name),
            // TODO: probably want to have more friendly names for simple struct refs.
            GpuType::Ref(inner) => {
                if let GpuType::InlineStruct(name) = inner.deref() {
                    format!("{}Ref", name)
                } else {
                    "uint".into()
                }
            }
        }
    }

    fn hlsl_typename(&self) -> String {
        match self {
            GpuType::Scalar(scalar) => scalar.hlsl_typename().into(),
            GpuType::Vector(scalar, size) => {
                match scalar {
                    GpuScalar::F32 | GpuScalar::I32 | GpuScalar::U32 => {
                        format!("{}{}", scalar.hlsl_typename(), size)
                    }
                    _ => {
                        let size_in_uints = size_in_uints(scalar.size() * size);
                        //TODO: where should sanity checks for size be done?
                        if size_in_uints == 1 {
                            String::from("uint")
                        } else {
                            format!("uint{}", size_in_uints)
                        }
                    }
                }
            }
            GpuType::InlineStruct(name) => name.to_string(),
            // TODO: probably want to have more friendly names for simple struct refs.
            GpuType::Ref(inner) => {
                if let GpuType::InlineStruct(name) = inner.deref() {
                    format!("{}Ref", name)
                } else {
                    "uint".into()
                }
            }
        }
    }

    fn size(&self, module: &GpuModule) -> usize {
        match self {
            GpuType::Scalar(scalar) => scalar.size(),
            GpuType::Vector(scalar, size) => scalar.size() * size,
            GpuType::InlineStruct(name) => module.resolve_by_name(&name).unwrap().size(module),
            GpuType::Ref(_name) => 4,
        }
    }

    fn alignment(&self, module: &GpuModule) -> usize {
        // TODO: there are alignment problems with vectors of 3
        match self {
            GpuType::Scalar(scalar) => scalar.size(),
            GpuType::Vector(scalar, size) => scalar.size() * size,
            GpuType::InlineStruct(name) => module.resolve_by_name(&name).unwrap().alignment(module),
            GpuType::Ref(_name) => 4,
        }
    }

    /// Report whether type is a scalar or simple vector
    fn is_small(&self) -> bool {
        match self {
            GpuType::Scalar(_) => true,
            GpuType::Vector(_, _) => true,
            GpuType::InlineStruct(_) => false,
            GpuType::Ref(_) => true,
        }
    }

    fn from_syn(ty: &syn::Type) -> Result<Self, String> {
        //println!("gputype {:#?}", ty);
        if let Some(scalar) = GpuScalar::from_syn(ty) {
            return Ok(GpuType::Scalar(scalar));
        }
        if let Some(name) = ty_as_single_ident(ty) {
            // Note: we're not doing any validation here.
            return Ok(GpuType::InlineStruct(name));
        }
        match ty {
            syn::Type::Path(TypePath {
                path: syn::Path { segments, .. },
                ..
            }) => {
                if segments.len() == 1 {
                    let seg = &segments[0];
                    if seg.ident == "Ref" {
                        if let PathArguments::AngleBracketed(args) = &seg.arguments {
                            if args.args.len() == 1 {
                                if let GenericArgument::Type(inner) = &args.args[0] {
                                    let inner_ty = GpuType::from_syn(inner)?;
                                    return Ok(GpuType::Ref(Box::new(inner_ty)));
                                }
                            }
                        }
                    }
                }
                Err("unknown path case".into())
            }
            syn::Type::Array(TypeArray { elem, len, .. }) => {
                if let Some(elem) = GpuScalar::from_syn(&elem) {
                    if let Some(len) = expr_int_lit(len) {
                        // maybe sanity-check length here
                        Ok(GpuType::Vector(elem, len))
                    } else {
                        Err("can't deal with variable length scalar arrays".into())
                    }
                } else {
                    Err("can't deal with non-scalar arrays".into())
                }
            }
            _ => Err("unknown type".into()),
        }
    }
}

impl GpuTypeDef {
    fn from_syn(item: &syn::Item) -> Result<Self, String> {
        match item {
            syn::Item::Struct(ItemStruct {
                ident,
                fields: Fields::Named(FieldsNamed { named, .. }),
                ..
            }) => {
                let mut fields = Vec::new();
                for field in named {
                    let field_ty = GpuType::from_syn(&field.ty)?;
                    let field_name = field.ident.as_ref().ok_or("need name".to_string())?;
                    fields.push((field_name.to_string(), field_ty));
                }
                Ok(GpuTypeDef::Struct(ident.to_string(), fields))
            }
            syn::Item::Enum(ItemEnum {
                ident, variants, ..
            }) => {
                let mut v = Vec::new();
                for variant in variants {
                    let vname = variant.ident.to_string();
                    let mut fields = Vec::new();
                    if let Fields::Unnamed(FieldsUnnamed { unnamed, .. }) = &variant.fields {
                        for field in unnamed {
                            fields.push(GpuType::from_syn(&field.ty)?);
                        }
                    }
                    v.push((vname, fields));
                }
                let en = GpuEnum {
                    name: ident.to_string(),
                    variants: v,
                };
                Ok(GpuTypeDef::Enum(en))
            }
            _ => {
                eprintln!("{:#?}", item);
                Err("unknown item".into())
            }
        }
    }

    fn name(&self) -> &str {
        match self {
            GpuTypeDef::Struct(name, _) => &name,
            GpuTypeDef::Enum(en) => &en.name,
        }
    }

    fn collect_refs(&self, enum_variants: &mut HashSet<String>) {
        if let GpuTypeDef::Enum(en) = self {
            for variant in &en.variants {
                if let Some(GpuType::InlineStruct(name)) = variant.1.first() {
                    enum_variants.insert(name.clone());
                }
            }
        }
    }

    /// Size of the body of the definition.
    fn size(&self, module: &GpuModule) -> usize {
        match self {
            GpuTypeDef::Struct(name, fields) => {
                let mut offset = 0;
                if module.enum_variants.contains(name) {
                    offset += 4;
                }
                for (_name, field) in fields {
                    offset += align_padding(offset, field.alignment(module));
                    offset += field.size(module);
                }
                offset
            }
            GpuTypeDef::Enum(en) => {
                let mut max_offset = 4;
                for (_name, fields) in &en.variants {
                    let mut offset = 4;
                    for field in fields {
                        if let GpuType::InlineStruct(_) = field {
                            if offset == 4 {
                                offset = 0;
                            }
                        }
                        // Alignment needs work :/
                        //offset += align_padding(offset, field.alignment(module));
                        offset += field.size(module);
                    }
                    max_offset = max_offset.max(offset);
                }
                max_offset
            }
        }
    }

    /// Alignment of the body of the definition.
    fn alignment(&self, module: &GpuModule) -> usize {
        match self {
            GpuTypeDef::Struct(name, fields) => {
                let mut alignment = 1;
                if module.enum_variants.contains(name) {
                    alignment = 4;
                }
                for (_name, field) in fields {
                    alignment = alignment.max(field.alignment(module));
                }
                alignment
            }
            GpuTypeDef::Enum(_en) => unimplemented!(),
        }
    }

    fn to_metal(&self, module: &GpuModule) -> String {
        let mut r = String::new();
        match self {
            GpuTypeDef::Struct(name, fields) => {
                let rn = format!("{}Ref", name);
                // The packed struct definition (is missing variable sized arrays)
                write!(r, "struct {}Packed {{\n", name).unwrap();
                if module.enum_variants.contains(name) {
                    write!(r, "    uint tag;\n").unwrap();
                }
                for (field_name, ty) in fields {
                    write!(r, "    {} {};\n", ty.metal_typename(), field_name).unwrap();
                }
                write!(r, "}};\n").unwrap();
                // Read of packed structure
                write!(
                    r,
                    "{}Packed {}_read(const device char *buf, {} ref) {{\n",
                    name, name, rn
                )
                .unwrap();
                write!(
                    r,
                    "    return *((const device {}Packed *)(buf + ref));\n",
                    name
                )
                .unwrap();
                write!(r, "}}\n").unwrap();
                // Unpacked field accessors
                for (field_name, ty) in fields {
                    if ty.is_small() {
                        let tn = ty.metal_typename();
                        write!(
                            r,
                            "{} {}_{}(const device char *buf, {} ref) {{\n",
                            tn, name, field_name, rn
                        )
                        .unwrap();
                        write!(
                            r,
                            "    return ((const device {}Packed *)(buf + ref))->{};\n",
                            name, field_name
                        )
                        .unwrap();
                        write!(r, "}}\n").unwrap();
                    }
                }
            }
            GpuTypeDef::Enum(en) => {
                let rn = format!("{}Ref", en.name);
                write!(r, "struct {} {{\n", en.name).unwrap();
                write!(r, "    uint tag;\n").unwrap();
                let size = self.size(module);
                let body_size = ((size + 3) >> 2) - 1;
                write!(r, "    uint body[{}];\n", body_size).unwrap();
                write!(r, "}};\n").unwrap();
                write!(
                    r,
                    "uint {}_tag(const device char *buf, {} ref) {{\n",
                    en.name, rn
                )
                .unwrap();
                write!(
                    r,
                    "    return ((const device {} *)(buf + ref))->tag;\n",
                    en.name
                )
                .unwrap();
                write!(r, "}}\n").unwrap();
                // TODO: current code base is 1-based, but we could switch to 0
                let mut tag = 1;
                for (name, _fields) in &en.variants {
                    write!(r, "#define {}_{} {}\n", en.name, name, tag).unwrap();
                    tag += 1;
                }
            }
        }
        r
    }

    fn to_hlsl(&self, module: &GpuModule) -> String {
        let mut r = String::new();

        match self {
            GpuTypeDef::Struct(name, fields) => {
                let structure = SpecifiedStruct::new(module, name, fields.clone());
                write!(r, "{}", structure.packed_form.to_hlsl(module)).unwrap();
                write!(r, "{}", structure.to_hlsl()).unwrap();
            }
            GpuTypeDef::Enum(en) => {
                let rn = format!("{}Ref", en.name);

                write!(r, "struct {} {{\n", en.name).unwrap();
                write!(r, "    uint tag;\n").unwrap();

                let size = self.size(module);
                println!("size: {}", size);
                // TODO: this sometimes predicts incorrect number of u32s needed to store body (differences with metal alignment)
                let body_size = ((size + 3) >> 2) - 1;

                write!(r, "    uint body[{}];\n", body_size).unwrap();
                write!(r, "}};\n").unwrap();
                write!(
                    r,
                    "inline uint {}_tag(ByteAddressBuffer buf, {} ref) {{\n",
                    en.name, rn
                )
                .unwrap();

                write!(r, "    uint result = buf.Load(ref);\n    return result;\n").unwrap();
                write!(r, "}}\n\n").unwrap();

                let quotient_in_u32x4 = size / (4 * GpuScalar::U32.size());
                let quotient_in_bytes = quotient_in_u32x4 * 16;
                let remainder_in_u32s = (size - quotient_in_bytes) / 4;

                write!(r, "{}", "inline void PietItem_read_into(ByteAddressBuffer src, uint src_ref, RWByteAddressBuffer dst, uint dst_ref) {\n").unwrap();
                for i in 0..quotient_in_u32x4 {
                    write!(
                        r,
                        "    uint4 group{} = src.Load4({});\n",
                        i,
                        simplified_add("src_ref", i * 16)
                    )
                    .unwrap();
                    write!(
                        r,
                        "    dst.Store4({}, group{});\n",
                        simplified_add("dst_ref", i * 16),
                        i,
                    )
                    .unwrap();
                }
                match remainder_in_u32s {
                    1 | 2 | 3 => {
                        write!(
                            r,
                            "\n    uint{} group{} = src.Load{}({});\n",
                            remainder_in_u32s,
                            quotient_in_u32x4,
                            remainder_in_u32s,
                            simplified_add("src_ref", quotient_in_u32x4 * 16)
                        )
                        .unwrap();
                        write!(
                            r,
                            "    dst.Store{}({}, group{});\n",
                            remainder_in_u32s,
                            simplified_add("dst_ref", quotient_in_u32x4 * 16),
                            quotient_in_u32x4,
                        )
                        .unwrap();
                    }
                    _ => {}
                }
                write!(r, "{}", "}\n\n").unwrap();
            }
        }
        r
    }
}

impl GpuModule {
    fn from_syn(module: &syn::ItemMod) -> Result<Self, String> {
        let name = module.ident.to_string();
        let mut defs = Vec::new();
        let mut enum_variants = HashSet::new();
        if let Some((_brace, items)) = &module.content {
            for item in items {
                let def = GpuTypeDef::from_syn(item)?;
                def.collect_refs(&mut enum_variants);
                defs.push(def);
            }
        }
        Ok(GpuModule {
            name,
            enum_variants,
            defs,
        })
    }

    fn resolve_by_name(&self, name: &str) -> Result<&GpuTypeDef, String> {
        for def in &self.defs {
            if def.name() == name {
                return Ok(&def);
            }
        }
        Err(format!("could not find {} in module", name))
    }

    fn to_metal(&self) -> String {
        let mut r = String::new();
        for def in &self.defs {
            write!(&mut r, "typedef uint {}Ref;\n", def.name()).unwrap();
        }
        for def in &self.defs {
            r.push_str(&def.to_metal(self));
        }
        r
    }

    fn to_hlsl(&self) -> String {
        let mut r = String::new();

        write!(&mut r, "{}", generate_hlsl_value_extractor(8)).unwrap();
        write!(&mut r, "{}", generate_hlsl_value_extractor(16)).unwrap();

        for def in &self.defs {
            match def {
                GpuTypeDef::Struct(name, _) => {
                    write!(&mut r, "typedef uint {}Ref;\n", def.name()).unwrap();
                    write!(&mut r, "typedef uint {}PackedRef;\n", def.name()).unwrap();
                }
                GpuTypeDef::Enum(_) => {
                    write!(&mut r, "typedef uint {}Ref;\n", def.name()).unwrap();
                }
            }
        }

        write!(&mut r, "\n").unwrap();
        for def in &self.defs {
            r.push_str(&def.to_hlsl(self));
        }

        for def in &self.defs {
            let name = def.name();
            if !(self.enum_variants.contains(name)) {
                write!(
                    r,
                    "#define {}_SIZE {}\n",
                    to_snake_case(name).to_uppercase(),
                    def.size(self)
                )
                .unwrap();
            }
            if let GpuTypeDef::Enum(en) = def {
                let mut tag: usize = 0;
                for (name, _fields) in &en.variants {
                    write!(r, "#define {}_{} {}\n", en.name, name, tag).unwrap();
                    tag += 1;
                }
            }
        }
        r
    }
}

// Probably don't need this, will use ItemMod instead.
#[derive(Debug)]
struct Items(Vec<syn::Item>);

fn ty_as_single_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(TypePath {
        path: syn::Path { segments, .. },
        ..
    }) = ty
    {
        if segments.len() == 1 {
            let seg = &segments[0];
            if seg.arguments == PathArguments::None {
                return Some(seg.ident.to_string());
            }
        }
    }
    None
}

fn expr_int_lit(e: &Expr) -> Option<usize> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Int(lit_int),
        ..
    }) = e
    {
        lit_int.base10_parse().ok()
    } else {
        None
    }
}

fn align_padding(offset: usize, alignment: usize) -> usize {
    offset.wrapping_neg() & (alignment - 1)
}

#[proc_macro]
pub fn piet_metal(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::ItemMod);
    //println!("input: {:#?}", input);
    let module = GpuModule::from_syn(&input).unwrap();
    let gen_metal_fn = format_ident!("gen_metal_{}", input.ident);
    let result = module.to_metal();
    let expanded = quote! {
        fn #gen_metal_fn() {
            println!("{}", #result);
        }
    };
    expanded.into()
}

#[proc_macro_derive(PietMetal)]
pub fn derive_piet_metal(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    derive_proc_metal_impl(input)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn derive_proc_metal_impl(input: syn::DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    println!("input: {:#?}", input);
    match &input.data {
        Data::Struct { .. } => {
            println!("it's a struct!");
        }
        _ => (),
    }
    let s = "this is a string";
    let expanded = quote! {
        fn foo() {
            println!("this was generated by proc macro: {}", #s);
        }
    };
    Ok(expanded)
}

#[proc_macro]
pub fn piet_hlsl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::ItemMod);
    //println!("input: {:#?}", input);
    let module = GpuModule::from_syn(&input).unwrap();
    let gen_hlsl_fn = format_ident!("gen_hlsl_{}", input.ident);
    let result = module.to_hlsl();
    let expanded = quote! {
        fn #gen_hlsl_fn() -> String{
            String::from(#result)
        }
    };
    expanded.into()
}

impl Parse for Items {
    fn parse(input: ParseStream) -> Result<Self, syn::Error> {
        let mut items = Vec::new();
        while !input.is_empty() {
            items.push(input.parse()?)
        }
        Ok(Items(items))
    }
}

fn to_snake_case(mut str: &str) -> String {
    let mut words = vec![];
    // Preserve leading underscores
    str = str.trim_start_matches(|c: char| {
        if c == '_' {
            words.push(String::new());
            true
        } else {
            false
        }
    });
    for s in str.split('_') {
        let mut last_upper = false;
        let mut buf = String::new();
        if s.is_empty() {
            continue;
        }
        for ch in s.chars() {
            if !buf.is_empty() && buf != "'" && ch.is_uppercase() && !last_upper {
                words.push(buf);
                buf = String::new();
            }
            last_upper = ch.is_uppercase();
            buf.extend(ch.to_lowercase());
        }
        words.push(buf);
    }
    words.join("_")
}
