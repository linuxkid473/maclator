// Real native/translated GPU regression; see tests/check_gpu_reflection.py.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int fail(const char *message, NSError *error) {
    fprintf(stderr, "FAIL: %s%s%s\n", message, error ? ": " : "", error ? error.localizedDescription.UTF8String : "");
    return 1;
}

static BOOL hasBuffer(NSArray<MTLArgument *> *arguments, NSUInteger index) {
    for (MTLArgument *argument in arguments)
        if (argument.type == MTLArgumentTypeBuffer && argument.index == index && argument.active) return YES;
    return NO;
}

int main(void) {
    @autoreleasepool {
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        if (!device) return fail("no Metal device", nil);
        printf("Metal device: %s\n", device.name.UTF8String);
        fflush(stdout);
        NSError *error = nil;
        NSString *source = @"#include <metal_stdlib>\nusing namespace metal;\n"
            "vertex float4 mcl_vertex(uint i [[vertex_id]]) {\n"
            "  const float2 p[3] = {float2(-1,-1), float2(3,-1), float2(-1,3)};\n"
            "  return float4(p[i],0,1);\n}\n"
            "fragment float4 mcl_fragment(constant float4 &color [[buffer(0)]]) { return color; }\n"
            "kernel void mcl_compute(device uint *out [[buffer(0)]], uint i [[thread_position_in_grid]]) { out[i] = i*17+3; }\n";
        id<MTLLibrary> library = [device newLibraryWithSource:source options:nil error:&error];
        if (!library || error) return fail("Metal shader library", error);
        id<MTLCommandQueue> queue = [device newCommandQueue];
        if (!queue) return fail("command queue", nil);

        MTLRenderPipelineDescriptor *descriptor = [MTLRenderPipelineDescriptor new];
        descriptor.vertexFunction = [library newFunctionWithName:@"mcl_vertex"];
        descriptor.fragmentFunction = [library newFunctionWithName:@"mcl_fragment"];
        descriptor.colorAttachments[0].pixelFormat = MTLPixelFormatRGBA8Unorm;
        id<MTLRenderPipelineState> pipeline = nil;
        // Repeat the same request: reflection must survive repeated pipeline creation too.
        for (unsigned pass = 0; pass < 2; pass++) {
            MTLRenderPipelineReflection *reflection = nil;
            error = nil;
            pipeline = [device newRenderPipelineStateWithDescriptor:descriptor
                options:MTLPipelineOptionArgumentInfo | MTLPipelineOptionBufferTypeInfo
                reflection:&reflection error:&error];
            if (!pipeline || error) return fail("render pipeline", error);
            if (!reflection || !hasBuffer(reflection.fragmentArguments, 0)) return fail("fragment buffer reflection", nil);
            printf("PASS: render reflection request %u (fragment buffer 0)\n", pass + 1);
            fflush(stdout);
        }
        error = nil;
        if (![device newRenderPipelineStateWithDescriptor:descriptor options:0 reflection:NULL error:NULL])
            return fail("NULL object output parameters", nil);

        MTLTextureDescriptor *textureDescriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:16 height:16 mipmapped:NO];
        textureDescriptor.storageMode = MTLStorageModePrivate;
        textureDescriptor.usage = MTLTextureUsageRenderTarget;
        id<MTLTexture> texture = [device newTextureWithDescriptor:textureDescriptor];
        float color[4] = {0.25f, 0.5f, 0.75f, 1.0f};
        id<MTLBuffer> colorBuffer = [device newBufferWithBytes:color length:sizeof color options:MTLResourceStorageModeShared];
        id<MTLBuffer> readback = [device newBufferWithLength:4096 options:MTLResourceStorageModeShared];
        if (!texture || !colorBuffer || !readback || !readback.contents) return fail("render resources", nil);
        memset(readback.contents, 0, 4096);
        MTLRenderPassDescriptor *renderPass = [MTLRenderPassDescriptor renderPassDescriptor];
        renderPass.colorAttachments[0].texture = texture;
        renderPass.colorAttachments[0].loadAction = MTLLoadActionClear;
        renderPass.colorAttachments[0].storeAction = MTLStoreActionStore;
        renderPass.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 0);
        id<MTLCommandBuffer> command = [queue commandBuffer];
        id<MTLRenderCommandEncoder> encoder = [command renderCommandEncoderWithDescriptor:renderPass];
        [encoder setRenderPipelineState:pipeline];
        [encoder setFragmentBuffer:colorBuffer offset:0 atIndex:0];
        [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
        [encoder endEncoding];
        id<MTLBlitCommandEncoder> blit = [command blitCommandEncoder];
        [blit copyFromTexture:texture sourceSlice:0 sourceLevel:0 sourceOrigin:MTLOriginMake(0,0,0)
            sourceSize:MTLSizeMake(16,16,1) toBuffer:readback destinationOffset:0
            destinationBytesPerRow:256 destinationBytesPerImage:4096];
        [blit endEncoding]; [command commit]; [command waitUntilCompleted];
        if (command.status != MTLCommandBufferStatusCompleted) return fail("render command", command.error);
        const uint8_t *pixels = readback.contents;
        const int expected[4] = {64,128,191,255};
        for (unsigned y = 0; y < 16; y++) for (unsigned x = 0; x < 16; x++) for (unsigned c = 0; c < 4; c++) {
            int actual = pixels[y*256+x*4+c];
            if (abs(actual-expected[c]) > 1) {
                fprintf(stderr,"FAIL: render pixel (%u,%u) channel %u = %d expected %d\n",x,y,c,actual,expected[c]);
                return 1;
            }
        }
        puts("PASS: hardware vertex + fragment draw, private texture, 256-pixel readback");

        id<MTLFunction> computeFunction = [library newFunctionWithName:@"mcl_compute"];
        MTLComputePipelineReflection *computeReflection = nil;
        error = nil;
        id<MTLComputePipelineState> computePipeline = [device newComputePipelineStateWithFunction:computeFunction
            options:MTLPipelineOptionArgumentInfo | MTLPipelineOptionBufferTypeInfo
            reflection:&computeReflection error:&error];
        if (!computePipeline || error || !computeReflection || !hasBuffer(computeReflection.arguments,0))
            return fail("compute pipeline reflection", error);
        id<MTLBuffer> output = [device newBufferWithLength:256*sizeof(uint32_t) options:MTLResourceStorageModeShared];
        memset(output.contents,0,256*sizeof(uint32_t));
        command = [queue commandBuffer];
        id<MTLComputeCommandEncoder> compute = [command computeCommandEncoder];
        [compute setComputePipelineState:computePipeline];
        [compute setBuffer:output offset:0 atIndex:0];
        [compute dispatchThreads:MTLSizeMake(256,1,1) threadsPerThreadgroup:MTLSizeMake(32,1,1)];
        [compute endEncoding]; [command commit]; [command waitUntilCompleted];
        if (command.status != MTLCommandBufferStatusCompleted) return fail("compute command",command.error);
        const uint32_t *values = output.contents;
        for (unsigned i = 0; i < 256; i++) if (values[i] != i*17+3) return fail("compute data",nil);
        puts("PASS: hardware compute reflection + 256-value readback");

        MTLRenderPipelineDescriptor *invalid = [MTLRenderPipelineDescriptor new];
        MTLRenderPipelineReflection *invalidReflection = nil;
        error = nil;
        id<MTLRenderPipelineState> bad = [device newRenderPipelineStateWithDescriptor:invalid
            options:MTLPipelineOptionArgumentInfo reflection:&invalidReflection error:&error];
        if (bad || invalidReflection || !error || !error.domain.length) return fail("separate pipeline error output",error);
        puts("PASS: separate NSError output on invalid pipeline");
        return 0;
    }
}
