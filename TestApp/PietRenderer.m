//  Copyright 2019 The xi-editor authors.

@import MetalKit;

#import "PietRenderer.h"
#import "PietShaderTypes.h"

@implementation PietRenderer {
    id<MTLDevice> _device;
    id<MTLComputePipelineState> _computePipelineState;
    id<MTLRenderPipelineState> _renderPipelineState;
    id<MTLCommandQueue> _commandQueue;
    id<MTLTexture> _texture;
    vector_uint2 _viewportSize;
}

- (nonnull instancetype)initWithMetalKitView:(nonnull MTKView *)mtkView {
    self = [super init];
    if (self) {
        NSError *error = NULL;
        _device = mtkView.device;
        // Note: consider sRGB here (though ultimately there are complex questions where in the
        // pipeline these conversions should be).
        mtkView.colorPixelFormat = MTLPixelFormatBGRA8Unorm;
        id<MTLLibrary> defaultLibrary = [_device newDefaultLibrary];
        MTLRenderPipelineDescriptor *pipelineDescriptor = [[MTLRenderPipelineDescriptor alloc] init];
        id<MTLFunction> kernelFunction = [defaultLibrary newFunctionWithName:@"renderKernel"];
        id<MTLFunction> vertexFunction = [defaultLibrary newFunctionWithName:@"vertexShader"];
        id<MTLFunction> fragmentFunction = [defaultLibrary newFunctionWithName:@"fragmentShader"];
        pipelineDescriptor.vertexFunction = vertexFunction;
        pipelineDescriptor.fragmentFunction = fragmentFunction;
        pipelineDescriptor.colorAttachments[0].pixelFormat = mtkView.colorPixelFormat;
        
        _computePipelineState = [_device newComputePipelineStateWithFunction:kernelFunction error:&error];
        if (!_computePipelineState) {
            NSLog(@"Failed to create compute pipeline state, error %@", error);
            return nil;
        }
        _renderPipelineState = [_device newRenderPipelineStateWithDescriptor:pipelineDescriptor error: &error];

        _commandQueue = [_device newCommandQueue];
    }
    return self;
}

- (void)drawInMTKView:(nonnull MTKView *)view {
    RenderVertex quadVertices[] = {
        //Viewport Positions, Texture Coordinates
        { {  1,  -1 }, { 1.f, 1.f } },
        { { -1,  -1 }, { 0.f, 1.f } },
        { { -1,   1 }, { 0.f, 0.f } },
        
        { {  1,  -1 }, { 1.f, 1.f } },
        { { -1,   1 }, { 0.f, 0.f } },
        { {  1,   1 }, { 1.f, 0.f } },
    };
    quadVertices[0].textureCoordinate.x = _viewportSize.x;
    quadVertices[0].textureCoordinate.y = _viewportSize.y;
    quadVertices[1].textureCoordinate.y = _viewportSize.y;
    quadVertices[3].textureCoordinate.x = _viewportSize.x;
    quadVertices[3].textureCoordinate.y = _viewportSize.y;
    quadVertices[5].textureCoordinate.x = _viewportSize.x;
    id<MTLCommandBuffer> commandBuffer = [_commandQueue commandBuffer];
    commandBuffer.label = @"RenderCommand";

    // Run compute shader.
    id<MTLComputeCommandEncoder> computeEncoder = [commandBuffer computeCommandEncoder];
    [computeEncoder setComputePipelineState:_computePipelineState];
    [computeEncoder setTexture:_texture atIndex:0];
    MTLSize threadgroupSize = MTLSizeMake(16, 16, 1);
    MTLSize threadgroupCount = MTLSizeMake(
                                           (_viewportSize.x + threadgroupSize.width - 1) / threadgroupSize.width,
                                           (_viewportSize.y + threadgroupSize.height - 1) / threadgroupSize.height,
                                           1);
    [computeEncoder dispatchThreadgroups:threadgroupCount threadsPerThreadgroup:threadgroupSize];
    [computeEncoder endEncoding];
    
    MTLRenderPassDescriptor *renderPassDescriptor = view.currentRenderPassDescriptor;
    if (renderPassDescriptor != nil) {
        id<MTLRenderCommandEncoder> renderEncoder = [commandBuffer renderCommandEncoderWithDescriptor:renderPassDescriptor];
        [renderEncoder setViewport:(MTLViewport){0.0, 0.0, _viewportSize.x, _viewportSize.y, -1.0, 1.0}];
        [renderEncoder setRenderPipelineState:_renderPipelineState];
        [renderEncoder setVertexBytes:quadVertices
                               length:sizeof(quadVertices)
                              atIndex:RenderVertexInputIndexVertices];
        [renderEncoder setFragmentTexture:_texture atIndex:0];
        [renderEncoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:6];
        [renderEncoder endEncoding];
        [commandBuffer presentDrawable:view.currentDrawable];
    }
    [commandBuffer commit];
}

- (void)mtkView:(nonnull MTKView *)view drawableSizeWillChange:(CGSize)size {
    _viewportSize.x = size.width;
    _viewportSize.y = size.height;
    // TODO: try not to allocate as wildly on smooth resize (maybe round up
    // the size).
    MTLTextureDescriptor *descriptor = [[MTLTextureDescriptor alloc] init];
    descriptor.textureType = MTLTextureType2D;
    descriptor.pixelFormat = MTLPixelFormatBGRA8Unorm;
    descriptor.width = _viewportSize.x;
    descriptor.height = _viewportSize.y;
    descriptor.usage = MTLTextureUsageShaderWrite | MTLTextureUsageShaderRead;
    _texture = [_device newTextureWithDescriptor:descriptor];
}

@end
