// Copyright 2016 The Gfx-rs Developers.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{mem, ptr};
use winapi;
use core::{self, texture as tex, memory};
use core::memory::Usage;
use command;
use {Buffer, Texture};


pub fn update_buffer(context: *mut winapi::ID3D11DeviceContext, buffer: &Buffer,
                     data: &[u8], offset_bytes: usize) {
    let dst_resource = (buffer.0).0 as *mut winapi::ID3D11Resource;
    match buffer.1 {
        Usage::Immutable | Usage::CpuOnly(memory::READ) => {
            error!("Unable to update an immutable buffer {:?}", buffer);
        },
        Usage::GpuOnly => {
            let dst_box = winapi::D3D11_BOX {
                left:   offset_bytes as winapi::UINT,
                top:    0,
                front:  0,
                right:  (offset_bytes + data.len()) as winapi::UINT,
                bottom: 1,
                back:   1,
            };
            let ptr = data.as_ptr() as *const _;
            unsafe {
                (*context).UpdateSubresource(dst_resource, 0, &dst_box, ptr, 0, 0)
            };
        },
        Usage::Persistent(_) => unimplemented!(),
        Usage::Dynamic | Usage::CpuOnly(_) => {
            let map_type = winapi::D3D11_MAP_WRITE_DISCARD;
            let hr = unsafe {
                let mut sub = mem::zeroed();
                let hr = (*context).Map(dst_resource, 0, map_type, 0, &mut sub);
                let dst = (sub.pData as *mut u8).offset(offset_bytes as isize);
                ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                (*context).Unmap(dst_resource, 0);
                hr
            };
            if !winapi::SUCCEEDED(hr) {
                error!("Buffer {:?} failed to map, error {:x}", buffer, hr);
            }
        },
    }
}

pub fn update_texture(context: *mut winapi::ID3D11DeviceContext, texture: &Texture, kind: tex::Kind,
                      face: Option<tex::CubeFace>, data: &[u8], image: &tex::RawImageInfo) {
    use core::texture::CubeFace::*;
    use winapi::UINT;

    let array_slice = match face {
        Some(PosX) => 0,
        Some(NegX) => 1,
        Some(PosY) => 2,
        Some(NegY) => 3,
        Some(PosZ) => 4,
        Some(NegZ) => 5,
        None => 0,
    };
    let num_mipmap_levels = 1; //TODO
    let subres = array_slice * num_mipmap_levels + (image.mipmap as UINT);
    let dst_resource = texture.to_resource();

    match texture.1 {
        Usage::Immutable | Usage::CpuOnly(memory::READ) => {
            error!("Unable to update an immutable texture {:?}", texture);
        },
        Usage::GpuOnly => {
            let (width, height, _, _) = kind.get_level_dimensions(image.mipmap);
            let stride = image.format.0.get_total_bits() as UINT;
            let row_pitch = width as UINT * stride;
            let depth_pitch = height as UINT * row_pitch;      
            let dst_box = winapi::D3D11_BOX {
                left:   image.xoffset as UINT,
                top:    image.yoffset as UINT,
                front:  image.zoffset as UINT,
                right:  (image.xoffset + image.width) as UINT,
                bottom: (image.yoffset + image.height) as UINT,
                back:   (image.zoffset + image.depth) as UINT,
            };
            let ptr = data.as_ptr() as *const _;
            unsafe {
                //let subres = winapi::D3D11CalcSubresource(image.mipmap, array_slice, num_mipmap_levels);
                (*context).UpdateSubresource(dst_resource, subres, &dst_box, ptr, row_pitch, depth_pitch)
            };
        },
        Usage::Dynamic | Usage::CpuOnly(_) | Usage::Persistent(_) => unimplemented!(),
    }
    
}


pub fn process(ctx: *mut winapi::ID3D11DeviceContext, command: &command::Command, data_buf: &command::DataBuffer) {
    use winapi::UINT;
    use core::shade::Stage;
    use command::Command::*;

    let max_cb  = core::MAX_CONSTANT_BUFFERS as UINT;
    let max_srv = core::MAX_RESOURCE_VIEWS   as UINT;
    let max_sm  = core::MAX_SAMPLERS         as UINT;
    debug!("Processing {:?}", command);
    match *command {
        BindProgram(ref prog) => unsafe {
            (*ctx).VSSetShader(prog.vs, ptr::null_mut(), 0);
            (*ctx).HSSetShader(prog.hs, ptr::null_mut(), 0);
            (*ctx).DSSetShader(prog.ds, ptr::null_mut(), 0);
            (*ctx).GSSetShader(prog.gs, ptr::null_mut(), 0);
            (*ctx).PSSetShader(prog.ps, ptr::null_mut(), 0);
        },
        BindInputLayout(layout) => unsafe {
            (*ctx).IASetInputLayout(layout);
        },
        BindIndex(ref buf, format) => unsafe {
            (*ctx).IASetIndexBuffer((buf.0).0, format, 0);
        },
        BindVertexBuffers(ref buffers, ref strides, ref offsets) => unsafe {
            (*ctx).IASetVertexBuffers(0, core::MAX_VERTEX_ATTRIBUTES as UINT,
                &buffers[0].0, strides.as_ptr(), offsets.as_ptr());
        },
        BindConstantBuffers(stage, ref buffers) => match stage {
            Stage::Vertex => unsafe {
                (*ctx).VSSetConstantBuffers(0, max_cb, &buffers[0].0);
            },
            Stage::Hull => unsafe {
                (*ctx).HSSetConstantBuffers(0, max_cb, &buffers[0].0);
            },
            Stage::Domain => unsafe {
                (*ctx).DSSetConstantBuffers(0, max_cb, &buffers[0].0);
            },
            Stage::Geometry => unsafe {
                (*ctx).GSSetConstantBuffers(0, max_cb, &buffers[0].0);
            },
            Stage::Pixel => unsafe {
                (*ctx).PSSetConstantBuffers(0, max_cb, &buffers[0].0);
            },
        },
        BindShaderResources(stage, ref views) => match stage {
            Stage::Vertex => unsafe {
                (*ctx).VSSetShaderResources(0, max_srv, &views[0].0);
            },
            Stage::Hull => unsafe {
                (*ctx).HSSetShaderResources(0, max_srv, &views[0].0);
            },
            Stage::Domain => unsafe {
                (*ctx).DSSetShaderResources(0, max_srv, &views[0].0);
            },
            Stage::Geometry => unsafe {
                (*ctx).GSSetShaderResources(0, max_srv, &views[0].0);
            },
            Stage::Pixel => unsafe {
                (*ctx).PSSetShaderResources(0, max_srv, &views[0].0);
            },
        },
        BindSamplers(stage, ref samplers) => match stage {
            Stage::Vertex => unsafe {
                (*ctx).VSSetSamplers(0, max_sm, &samplers[0].0);
            },
            Stage::Hull => unsafe {
                (*ctx).HSSetSamplers(0, max_sm, &samplers[0].0);
            },
            Stage::Domain => unsafe {
                (*ctx).DSSetSamplers(0, max_sm, &samplers[0].0);
            },
            Stage::Geometry => unsafe {
                (*ctx).GSSetSamplers(0, max_sm, &samplers[0].0);
            },
            Stage::Pixel => unsafe {
                (*ctx).PSSetSamplers(0, max_sm, &samplers[0].0);
            },
        },
        BindPixelTargets(ref colors, ds) => unsafe {
            (*ctx).OMSetRenderTargets(core::MAX_COLOR_TARGETS as UINT,
                &colors[0].0, ds.0);
        },
        SetPrimitive(topology) => unsafe {
            (*ctx).IASetPrimitiveTopology(topology);
        },
        SetViewport(ref viewport) => unsafe {
            (*ctx).RSSetViewports(1, viewport);
        },
        SetScissor(ref rect) => unsafe {
            (*ctx).RSSetScissorRects(1, rect);
        },
        SetRasterizer(rast) => unsafe {
            (*ctx).RSSetState(rast as *mut _);
        },
        SetDepthStencil(ds, value) => unsafe {
            (*ctx).OMSetDepthStencilState(ds as *mut _, value);
        },
        SetBlend(blend, ref value, mask) => unsafe {
            (*ctx).OMSetBlendState(blend as *mut _, value, mask);
        },
        UpdateBuffer(ref buffer, pointer, offset) => {
            let data = data_buf.get(pointer);
            update_buffer(ctx, buffer, data, offset);
        },
        UpdateTexture(ref tex, kind, face, pointer, ref image) => {
            let data = data_buf.get(pointer);
            update_texture(ctx, tex, kind, face, data, image);
        },
        GenerateMips(ref srv) => unsafe {
            (*ctx).GenerateMips(srv.0);
        },
        ClearColor(target, ref data) => unsafe {
            (*ctx).ClearRenderTargetView(target.0, data);
        },
        ClearDepthStencil(target, flags, depth, stencil) => unsafe {
            (*ctx).ClearDepthStencilView(target.0, flags.0, depth, stencil);
        },
        Draw(nvert, svert) => unsafe {
            (*ctx).Draw(nvert, svert);
        },
        DrawInstanced(nvert, ninst, svert, sinst) => unsafe {
            (*ctx).DrawInstanced(nvert, ninst, svert, sinst);
        },
        DrawIndexed(nind, svert, base) => unsafe {
            (*ctx).DrawIndexed(nind, svert, base);
        },
        DrawIndexedInstanced(nind, ninst, sind, base, sinst) => unsafe {
            (*ctx).DrawIndexedInstanced(nind, ninst, sind, base, sinst);
        },
    }
}
