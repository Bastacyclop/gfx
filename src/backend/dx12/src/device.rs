use conv;
use core::{buffer, device as d, format, image, mapping, memory, pass, pso, state};
use core::{Features, HeapType, Limits};
use core::memory::Requirements;
use d3d12;
use d3dcompiler;
use dxguid;
use kernel32;
use std::cmp;
use std::collections::BTreeMap;
use std::ops::Range;
use std::{ffi, mem, ptr, slice};
use {native as n, shade, Backend as B, Device};
use winapi;
use wio::com::ComPtr;

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct Mapping;

#[derive(Debug)]
pub struct UnboundBuffer {
    requirements: memory::Requirements,
    stride: u64,
    usage: buffer::Usage,
}

#[derive(Debug)]
pub struct UnboundImage {
    desc: winapi::D3D12_RESOURCE_DESC,
    requirements: memory::Requirements,
    kind: image::Kind,
    usage: image::Usage,
    bits_per_texel: u8,
    levels: image::Level,
}

impl Device {
    pub fn create_shader_library(
        &mut self,
        shaders: &[(pso::EntryPoint, &[u8])],
    ) -> Result<n::ShaderLib, pso::CreateShaderError> {
        let mut shader_map = BTreeMap::new();
        // TODO: handle entry points with the same name
        for &(entry_point, byte_code) in shaders {
            let mut blob: *mut winapi::ID3DBlob = ptr::null_mut();
            let hr = unsafe {
                d3dcompiler::D3DCreateBlob(
                    byte_code.len() as u64,
                    &mut blob as *mut *mut _)
            };
            if !winapi::SUCCEEDED(hr) {
                error!("D3DCreateBlob error {:x}", hr);
                let message = "D3DCreateBlob fail".to_string();
                return Err(pso::CreateShaderError::CompilationFailed(message))
            }

            unsafe {
                ptr::copy(
                    byte_code.as_ptr(),
                    (*blob).GetBufferPointer() as *mut u8,
                    byte_code.len());
            }
            shader_map.insert(entry_point, blob);
        }
        Ok(n::ShaderLib { shaders: shader_map })
    }

    pub fn create_shader_library_from_source(
        &mut self,
        shaders: &[(pso::EntryPoint, pso::Stage, &[u8])],
    ) -> Result<n::ShaderLib, pso::CreateShaderError> {
        let stage_to_str = |stage| {
            match stage {
                pso::Stage::Vertex => "vs_5_0\0",
                pso::Stage::Pixel => "ps_5_0\0",
                _ => unimplemented!(),
            }
        };

        let mut shader_map = BTreeMap::new();
        // TODO: handle entry points with the same name
        for &(entry_point, stage, byte_code) in shaders {
            let mut blob = ptr::null_mut();
            let mut error = ptr::null_mut();
            let entry = ffi::CString::new(entry_point).unwrap();
            let hr = unsafe {
                d3dcompiler::D3DCompile(
                    byte_code.as_ptr() as *const _,
                    byte_code.len() as u64,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    entry.as_ptr() as *const _,
                    stage_to_str(stage).as_ptr() as *const i8,
                    1,
                    0,
                    &mut blob as *mut *mut _,
                    &mut error as *mut *mut _)
            };
            if !winapi::SUCCEEDED(hr) {
                error!("D3DCompile error {:x}", hr);
                let mut error = unsafe { ComPtr::<winapi::ID3DBlob>::new(error) };
                let message = unsafe {
                    let pointer = error.GetBufferPointer();
                    let size = error.GetBufferSize();
                    let slice = slice::from_raw_parts(pointer as *const u8, size as usize);
                    String::from_utf8_lossy(slice).into_owned()
                };
                return Err(pso::CreateShaderError::CompilationFailed(message))
            }

            shader_map.insert(entry_point, blob);
        }
        Ok(n::ShaderLib { shaders: shader_map })
    }

    pub fn create_descriptor_heap_impl(
        device: &mut ComPtr<winapi::ID3D12Device>,
        heap_type: winapi::D3D12_DESCRIPTOR_HEAP_TYPE,
        shader_visible: bool,
        capacity: usize,
    ) -> n::DescriptorHeap {
        let desc = winapi::D3D12_DESCRIPTOR_HEAP_DESC {
            Type: heap_type,
            NumDescriptors: capacity as u32,
            Flags: if shader_visible {
                winapi::D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
            } else {
                winapi::D3D12_DESCRIPTOR_HEAP_FLAG_NONE
            },
            NodeMask: 0,
        };

        let mut heap: *mut winapi::ID3D12DescriptorHeap = ptr::null_mut();
        let mut cpu_handle = winapi::D3D12_CPU_DESCRIPTOR_HANDLE { ptr: 0 };
        let mut gpu_handle = winapi::D3D12_GPU_DESCRIPTOR_HANDLE { ptr: 0 };
        let descriptor_size = unsafe {
            device.CreateDescriptorHeap(
                &desc,
                &dxguid::IID_ID3D12DescriptorHeap,
                &mut heap as *mut *mut _ as *mut *mut _,
            );
            (*heap).GetCPUDescriptorHandleForHeapStart(&mut cpu_handle);
            (*heap).GetGPUDescriptorHandleForHeapStart(&mut gpu_handle);
            device.GetDescriptorHandleIncrementSize(heap_type) as u64
        };

        n::DescriptorHeap {
            raw: unsafe { ComPtr::new(heap) },
            handle_size: descriptor_size,
            total_handles: capacity as u64,
            start: n::DualHandle {
                cpu: cpu_handle,
                gpu: gpu_handle,
            },
        }
    }
}

impl d::Device<B> for Device {
    fn get_features(&self) -> &Features { &self.features }
    fn get_limits(&self) -> &Limits { &self.limits }

    fn create_heap(
        &mut self,
        heap_type: &HeapType,
        resource_type: d::ResourceHeapType,
        size: u64,
    ) -> Result<n::Heap, d::ResourceHeapError> {
        let mut heap = ptr::null_mut();

        let flags = match resource_type {
            d::ResourceHeapType::Any if !self.features.heterogeneous_resource_heaps => return Err(d::ResourceHeapError::UnsupportedType),
            d::ResourceHeapType::Any => winapi::D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES,
            d::ResourceHeapType::Buffers => winapi::D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
            d::ResourceHeapType::Images  => winapi::D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            d::ResourceHeapType::Targets => winapi::D3D12_HEAP_FLAG_ALLOW_ONLY_RT_DS_TEXTURES,
        };

        let desc = winapi::D3D12_HEAP_DESC {
            SizeInBytes: size,
            Properties: conv::map_heap_properties(heap_type.properties),
            Alignment: 0, //Warning: has to be 4K for MSAA targets
            Flags: flags,
        };

        let hr = unsafe {
            self.device.CreateHeap(&desc, &dxguid::IID_ID3D12Heap, &mut heap)
        };
        if hr == winapi::E_OUTOFMEMORY {
            return Err(d::ResourceHeapError::OutOfMemory);
        }
        assert_eq!(winapi::S_OK, hr);

        //TODO: merge with `map_heap_properties`
        let default_state = if !heap_type.properties.contains(memory::CPU_VISIBLE) {
            winapi::D3D12_RESOURCE_STATE_COMMON
        } else if heap_type.properties.contains(memory::COHERENT) {
            winapi::D3D12_RESOURCE_STATE_GENERIC_READ
        } else {
            winapi::D3D12_RESOURCE_STATE_COPY_DEST
        };

        Ok(n::Heap {
            raw: unsafe { ComPtr::new(heap as _) },
            ty: heap_type.clone(),
            size,
            default_state,
        })
    }

    fn create_renderpass(
        &mut self,
        attachments: &[pass::Attachment],
        subpasses: &[pass::SubpassDesc],
        _dependencies: &[pass::SubpassDependency],
    ) -> n::RenderPass {
        // TODO:
        let subpasses = subpasses
            .iter()
            .map(|subpass| {
                n::SubpassDesc {
                    color_attachments: subpass.color_attachments.iter().cloned().collect(),
                }
            }).collect();

        n::RenderPass {
            attachments: attachments.to_vec(),
            subpasses,
        }
    }

    fn create_pipeline_layout(&mut self, sets: &[&n::DescriptorSetLayout]) -> n::PipelineLayout {
        let total = sets.iter().map(|desc_sec| desc_sec.bindings.len()).sum();
        // guarantees that no re-allocation is done, and our pointers are valid
        let mut ranges = Vec::with_capacity(total);

        let parameters = sets.iter().map(|desc_set| {
            let mut param = winapi::D3D12_ROOT_PARAMETER {
                ParameterType: winapi::D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                ShaderVisibility: winapi::D3D12_SHADER_VISIBILITY_ALL, //TODO
                .. unsafe { mem::zeroed() }
            };
            let range_base = ranges.len();
            ranges.extend(desc_set.bindings.iter().map(|bind| winapi::D3D12_DESCRIPTOR_RANGE {
                RangeType: match bind.ty {
                    pso::DescriptorType::Sampler => winapi::D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
                    pso::DescriptorType::SampledImage => winapi::D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                    pso::DescriptorType::StorageBuffer |
                    pso::DescriptorType::StorageImage => winapi::D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                    pso::DescriptorType::ConstantBuffer => winapi::D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
                    _ => panic!("unsupported binding type {:?}", bind.ty)
                },
                NumDescriptors: bind.count as u32,
                BaseShaderRegister: bind.binding as u32,
                RegisterSpace: 0, //TODO?
                OffsetInDescriptorsFromTableStart: winapi::D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            }));
            ranges[0].OffsetInDescriptorsFromTableStart = 0; //careful!
            *unsafe{ param.DescriptorTable_mut() } = winapi::D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: (ranges.len() - range_base) as u32,
                pDescriptorRanges: ranges[range_base..].as_ptr(),
            };
            param
        }).collect::<Vec<_>>();

        let desc = winapi::D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: parameters.len() as u32,
            pParameters: parameters.as_ptr(),
            NumStaticSamplers: 0,
            pStaticSamplers: ptr::null(),
            Flags: winapi::D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
        };

        let mut signature = ptr::null_mut();
        let mut signature_raw = ptr::null_mut();

        // TODO: error handling
        unsafe {
            d3d12::D3D12SerializeRootSignature(
                &desc,
                winapi::D3D_ROOT_SIGNATURE_VERSION_1,
                &mut signature_raw,
                ptr::null_mut(),
            );

            self.device.CreateRootSignature(
                0,
                (*signature_raw).GetBufferPointer(),
                (*signature_raw).GetBufferSize(),
                &dxguid::IID_ID3D12RootSignature,
                &mut signature as *mut *mut _ as *mut *mut _);
        }
        unsafe { (*signature_raw).Release() };

        n::PipelineLayout { raw: signature }
    }

    fn create_graphics_pipelines<'a>(
        &mut self,
        descs: &[(&n::ShaderLib, &n::PipelineLayout, pass::Subpass<'a, B>, &pso::GraphicsPipelineDesc)],
    ) -> Vec<Result<n::GraphicsPipeline, pso::CreationError>> {
        descs.iter().map(|&(shader_lib, ref signature, ref subpass, ref desc)| {
            let build_shader = |lib: &n::ShaderLib, entry: Option<pso::EntryPoint>| {
                // TODO: better handle case where looking up shader fails
                let shader = entry.and_then(|entry| lib.shaders.get(entry));
                match shader {
                    Some(shader) => {
                        winapi::D3D12_SHADER_BYTECODE {
                            pShaderBytecode: unsafe { (**shader).GetBufferPointer() as *const _ },
                            BytecodeLength: unsafe { (**shader).GetBufferSize() as u64 },
                        }
                    }
                    None => {
                        winapi::D3D12_SHADER_BYTECODE {
                            pShaderBytecode: ptr::null(),
                            BytecodeLength: 0,
                        }
                    }
                }
            };

            let vs = build_shader(shader_lib, Some(desc.shader_entries.vertex_shader));
            let ps = build_shader(shader_lib, desc.shader_entries.pixel_shader);
            let gs = build_shader(shader_lib, desc.shader_entries.geometry_shader);
            let ds = build_shader(shader_lib, desc.shader_entries.domain_shader);
            let hs = build_shader(shader_lib, desc.shader_entries.hull_shader);

            // Define input element descriptions
            let mut vs_reflect = shade::reflect_shader(&vs);
            let input_element_descs = {
                let input_descs = shade::reflect_input_elements(&mut vs_reflect);
                desc.attributes
                    .iter()
                    .map(|attrib| {
                        let buffer_desc = if let Some(buffer_desc) = desc.vertex_buffers.get(attrib.binding as usize) {
                                buffer_desc
                            } else {
                                error!("Couldn't find associated vertex buffer description {:?}", attrib.binding);
                                return Err(pso::CreationError::Other);
                            };

                        let input_elem =
                            if let Some(input_elem) = input_descs.iter().find(|elem| elem.semantic_index == attrib.location) {
                                input_elem
                            } else {
                                error!("Couldn't find associated input element slot in the shader {:?}", attrib.location);
                                return Err(pso::CreationError::Other);
                            };

                        let slot_class = match buffer_desc.rate {
                            0 => winapi::D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                            _ => winapi::D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                        };
                        let format = attrib.element.format;

                        Ok(winapi::D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: input_elem.semantic_name,
                            SemanticIndex: input_elem.semantic_index,
                            Format: match conv::map_format(format, false) {
                                Some(fm) => fm,
                                None => {
                                    error!("Unable to find DXGI format for {:?}", format);
                                    return Err(pso::CreationError::Other);
                                }
                            },
                            InputSlot: attrib.binding as _,
                            AlignedByteOffset: attrib.element.offset,
                            InputSlotClass: slot_class,
                            InstanceDataStepRate: buffer_desc.rate as _,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };

            // TODO: check maximum number of rtvs
            // Get associated subpass information
            let pass = match subpass.main_pass.subpasses.get(subpass.index) {
                Some(subpass) => subpass,
                None => return Err(pso::CreationError::InvalidSubpass(subpass.index)),
            };

            // Get color attachment formats from subpass
            let (rtvs, num_rtvs) = {
                let mut rtvs = [winapi::DXGI_FORMAT_UNKNOWN; 8];
                let mut num_rtvs = 0;
                for (mut rtv, target) in rtvs.iter_mut()
                                             .zip(pass.color_attachments.iter())
                {
                    let format = subpass.main_pass.attachments[target.0].format;
                    println!("{:?}", format);
                    *rtv = conv::map_format(format, true).unwrap_or(winapi::DXGI_FORMAT_UNKNOWN);
                    num_rtvs += 1;
                }
                (rtvs, num_rtvs)
            };

            // Setup pipeline description
            let pso_desc = winapi::D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: signature.raw,
                VS: vs, PS: ps, GS: gs, DS: ds, HS: hs,
                StreamOutput: winapi::D3D12_STREAM_OUTPUT_DESC {
                    pSODeclaration: ptr::null(),
                    NumEntries: 0,
                    pBufferStrides: ptr::null(),
                    NumStrides: 0,
                    RasterizedStream: 0,
                },
                BlendState: winapi::D3D12_BLEND_DESC {
                    AlphaToCoverageEnable: if desc.blender.alpha_coverage { winapi::TRUE } else { winapi::FALSE },
                    IndependentBlendEnable: winapi::TRUE,
                    RenderTarget: conv::map_render_targets(&desc.blender.targets),
                },
                SampleMask: winapi::UINT::max_value(),
                RasterizerState: conv::map_rasterizer(&desc.rasterizer),
                DepthStencilState: conv::map_depth_stencil(
                    &match desc.depth_stencil {
                        Some((_, info)) => info,
                        None => pso::DepthStencilInfo {
                            depth: None,
                            front: None,
                            back: None,
                        }
                    }),
                InputLayout: winapi::D3D12_INPUT_LAYOUT_DESC {
                    pInputElementDescs: input_element_descs.as_ptr(),
                    NumElements: input_element_descs.len() as u32,
                },
                IBStripCutValue: winapi::D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
                PrimitiveTopologyType: conv::map_topology_type(desc.input_assembler.primitive),
                NumRenderTargets: num_rtvs,
                RTVFormats: rtvs,
                DSVFormat: desc.depth_stencil.and_then(|(format, _)| conv::map_format(format, true))
                                             .unwrap_or(winapi::DXGI_FORMAT_UNKNOWN),
                SampleDesc: winapi::DXGI_SAMPLE_DESC {
                    Count: 1, // TODO
                    Quality: 0, // TODO
                },
                NodeMask: 0,
                CachedPSO: winapi::D3D12_CACHED_PIPELINE_STATE {
                    pCachedBlob: ptr::null(),
                    CachedBlobSizeInBytes: 0,
                },
                Flags: winapi::D3D12_PIPELINE_STATE_FLAG_NONE,
            };

            let topology = conv::map_topology(desc.input_assembler.primitive);

            // Create PSO
            let mut pipeline = ptr::null_mut();
            let hr = unsafe {
                self.device.CreateGraphicsPipelineState(
                    &pso_desc,
                    &dxguid::IID_ID3D12PipelineState,
                    &mut pipeline as *mut *mut _ as *mut *mut _)
            };

            if winapi::SUCCEEDED(hr) {
                Ok(n::GraphicsPipeline { raw: pipeline, topology })
            } else {
                Err(pso::CreationError::Other)
            }
        }).collect()
    }

    fn create_compute_pipelines(
        &mut self,
        _descs: &[(&n::ShaderLib, pso::EntryPoint, &n::PipelineLayout)],
    ) -> Vec<Result<n::ComputePipeline, pso::CreationError>> {
        unimplemented!()
    }

    fn create_framebuffer(
        &mut self,
        _renderpass: &n::RenderPass,
        color_attachments: &[&n::RenderTargetView],
        depth_stencil_attachments: &[&n::DepthStencilView],
        _extent: d::Extent,
    ) -> n::FrameBuffer {
        n::FrameBuffer {
            color: color_attachments.iter().map(|rtv| **rtv).collect(),
            depth_stencil: depth_stencil_attachments.iter().map(|dsv| **dsv).collect(),
        }
    }

    fn create_sampler(&mut self, info: image::SamplerInfo) -> n::Sampler {
        let handle = self.sampler_pool.alloc_handles(1).cpu;

        let op = match info.comparison {
            Some(_) => conv::FilterOp::Comparison,
            None => conv::FilterOp::Product,
        };
        let desc = winapi::D3D12_SAMPLER_DESC {
            Filter: conv::map_filter(info.filter, op),
            AddressU: conv::map_wrap(info.wrap_mode.0),
            AddressV: conv::map_wrap(info.wrap_mode.1),
            AddressW: conv::map_wrap(info.wrap_mode.2),
            MipLODBias: info.lod_bias.into(),
            MaxAnisotropy: match info.filter {
                image::FilterMethod::Anisotropic(max) => max as _, // TODO: check support here?
                _ => 0,
            },
            ComparisonFunc: conv::map_function(info.comparison.unwrap_or(state::Comparison::Always)),
            BorderColor: info.border.into(),
            MinLOD: info.lod_range.start.into(),
            MaxLOD: info.lod_range.end.into(),
        };

        unsafe {
            self.device.CreateSampler(&desc, handle);
        }

        n::Sampler { handle }
    }

    fn create_buffer(
        &mut self,
        size: u64,
        stride: u64,
        usage: buffer::Usage,
    ) -> Result<UnboundBuffer, buffer::CreationError> {
        let requirements = memory::Requirements {
            size,
            alignment: winapi::D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64,
        };

        Ok(UnboundBuffer {
            requirements,
            stride,
            usage,
        })
    }

    fn get_buffer_requirements(&mut self, _buffer: &UnboundBuffer) -> Requirements {
        unimplemented!()
    }

    fn bind_buffer_memory(
        &mut self,
        heap: &n::Heap,
        offset: u64,
        buffer: UnboundBuffer,
    ) -> Result<n::Buffer, buffer::CreationError> {
        if offset + buffer.requirements.size > heap.size {
            return Err(buffer::CreationError::Other)
        }

        let mut resource = ptr::null_mut();
        let init_state = heap.default_state; //TODO?
        let desc = winapi::D3D12_RESOURCE_DESC {
            Dimension: winapi::D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: buffer.requirements.size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: winapi::DXGI_FORMAT_UNKNOWN,
            SampleDesc: winapi::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: winapi::D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: winapi::D3D12_RESOURCE_FLAGS(0),
        };

        assert_eq!(winapi::S_OK, unsafe {
            self.device.CreatePlacedResource(
                heap.raw.as_mut(),
                offset,
                &desc,
                init_state,
                ptr::null(),
                &dxguid::IID_ID3D12Resource,
                &mut resource,
            )
        });
        Ok(n::Buffer {
            resource: resource as *mut _,
            size_in_bytes: buffer.requirements.size as _,
            stride: buffer.stride as _,
        })
    }

    fn create_image(
        &mut self,
        kind: image::Kind,
        mip_levels: image::Level,
        format: format::Format,
        usage: image::Usage,
    ) -> Result<UnboundImage, image::CreationError> {
        let (width, height, depth, aa) = kind.get_dimensions();
        let dimension = match kind {
            image::Kind::D1(..) |
            image::Kind::D1Array(..) => winapi::D3D12_RESOURCE_DIMENSION_TEXTURE1D,
            image::Kind::D2(..) |
            image::Kind::D2Array(..) => winapi::D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            image::Kind::D3(..) |
            image::Kind::Cube(..) |
            image::Kind::CubeArray(..) => winapi::D3D12_RESOURCE_DIMENSION_TEXTURE3D,
        };
        let desc = winapi::D3D12_RESOURCE_DESC {
            Dimension: dimension,
            Alignment: 0,
            Width: width as u64,
            Height: height as u32,
            DepthOrArraySize: cmp::max(1, depth),
            MipLevels: mip_levels as u16,
            Format: match conv::map_format(format, false) {
                Some(format) => format,
                None => return Err(image::CreationError::Format(format.0, Some(format.1))),
            },
            SampleDesc: winapi::DXGI_SAMPLE_DESC {
                Count: aa.get_num_fragments() as u32,
                Quality: 0,
            },
            Layout: winapi::D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: winapi::D3D12_RESOURCE_FLAGS(0),
        };

        let mut alloc_info = unsafe { mem::zeroed() };
        unsafe {
            self.device.GetResourceAllocationInfo(&mut alloc_info, 0, 1, &desc);
        }

        Ok(UnboundImage {
            desc,
            requirements: memory::Requirements {
                size: alloc_info.SizeInBytes,
                alignment: alloc_info.Alignment,
            },
            kind,
            usage,
            bits_per_texel: format.0.get_total_bits(),
            levels: mip_levels,
        })
    }

    fn get_image_requirements(&mut self, image: &UnboundImage) -> Requirements {
        image.requirements
    }

    fn bind_image_memory(
        &mut self,
        heap: &n::Heap,
        offset: u64,
        image: UnboundImage,
    ) -> Result<n::Image, image::CreationError> {
        if offset + image.requirements.size > heap.size {
            return Err(image::CreationError::OutOfHeap)
        }

        let mut resource = ptr::null_mut();
        let init_state = heap.default_state; //TODO?

        assert_eq!(winapi::S_OK, unsafe {
            self.device.CreatePlacedResource(
                heap.raw.as_mut(),
                offset,
                &image.desc,
                init_state,
                ptr::null(),
                &dxguid::IID_ID3D12Resource,
                &mut resource,
            )
        });
        Ok(n::Image {
            resource: resource as *mut _,
            kind: image.kind,
            dxgi_format: image.desc.Format,
            bits_per_texel: image.bits_per_texel,
            levels: image.levels,
        })
    }

    fn view_buffer_as_constant(
        &mut self,
        _buffer: &n::Buffer,
        _range: Range<u64>,
    ) -> Result<n::ConstantBufferView, d::TargetViewError> {
        unimplemented!()
    }

    fn view_image_as_render_target(&mut self,
        image: &n::Image,
        format: format::Format,
        range: image::SubresourceRange,
    ) -> Result<n::RenderTargetView, d::TargetViewError> {
        let handle = self.rtv_pool.alloc_handles(1).cpu;

        if image.kind.get_dimensions().3 != image::AaMode::Single {
            error!("No MSAA supported yet!");
        }

        let mut desc = winapi::D3D12_RENDER_TARGET_VIEW_DESC {
            Format: match conv::map_format(format, true) {
                Some(format) => format,
                None => return Err(d::TargetViewError::BadFormat)
            },
            .. unsafe { mem::zeroed() }
        };

        match image.kind {
            image::Kind::D2(..) => {
                desc.ViewDimension = winapi::D3D12_RTV_DIMENSION_TEXTURE2D;
                *unsafe { desc.Texture2D_mut() } = winapi::D3D12_TEX2D_RTV {
                    MipSlice: 0,
                    PlaneSlice: 0,
                };
            },
            _ => unimplemented!()
        };

        unsafe {
            self.device.CreateRenderTargetView(
                image.resource,
                &desc,
                handle,
            );
        }

        Ok(n::RenderTargetView { handle })
    }

    fn view_image_as_shader_resource(
        &mut self,
        image: &n::Image,
        format: format::Format,
    ) -> Result<n::ShaderResourceView, d::TargetViewError> {
        let handle = self.srv_pool.alloc_handles(1).cpu;

        let dimension = match image.kind {
            image::Kind::D1(..) |
            image::Kind::D1Array(..) => winapi::D3D12_SRV_DIMENSION_TEXTURE1D,
            image::Kind::D2(..) |
            image::Kind::D2Array(..) => winapi::D3D12_SRV_DIMENSION_TEXTURE2D,
            image::Kind::D3(..) |
            image::Kind::Cube(..) |
            image::Kind::CubeArray(..) => winapi::D3D12_SRV_DIMENSION_TEXTURE3D,
        };

        let mut desc = winapi::D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: match conv::map_format(format, false) {
                Some(format) => format,
                None => return Err(d::TargetViewError::BadFormat),
            },
            ViewDimension: dimension,
            Shader4ComponentMapping: 0x1688, // TODO: map swizzle
            u: unsafe { mem::zeroed() },
        };

        match image.kind {
            image::Kind::D2(_, _, image::AaMode::Single) => {
                *unsafe{ desc.Texture2D_mut() } = winapi::D3D12_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: !0,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 0.0,
                }
            }
            _ => unimplemented!()
        }

        unsafe {
            self.device.CreateShaderResourceView(
                image.resource,
                &desc,
                handle,
            );
        }

        Ok(n::ShaderResourceView { handle })
    }

    fn view_image_as_unordered_access(
        &mut self,
        _image: &n::Image,
        _format: format::Format,
    ) -> Result<n::UnorderedAccessView, d::TargetViewError> {
        unimplemented!()
    }

    fn create_descriptor_pool(
        &mut self,
        max_sets: usize,
        descriptor_pools: &[pso::DescriptorRangeDesc],
    ) -> n::DescriptorPool {
        let offset = 0; // TODO
        warn!("Heap slice allocation not implemented for descriptor pools!");

        n::DescriptorPool {
            heap_srv_cbv_uav: self.heap_srv_cbv_uav.clone(),
            heap_sampler: self.heap_sampler.clone(),
            pools: descriptor_pools.to_vec(),
            max_size: max_sets as _,
            offset: offset as _,
        }
    }

    fn create_descriptor_set_layout(
        &mut self,
        bindings: &[pso::DescriptorSetLayoutBinding],
    )-> n::DescriptorSetLayout {
        n::DescriptorSetLayout { bindings: bindings.to_vec() }
    }

    fn update_descriptor_sets(&mut self, _writes: &[pso::DescriptorSetWrite<B>]) {
        unimplemented!()
    }

    fn read_mapping_raw(
        &mut self,
        _buf: &n::Buffer,
        _range: Range<u64>,
    ) -> Result<(*const u8, Mapping), mapping::Error> {
        unimplemented!()
    }

    fn write_mapping_raw(
        &mut self,
        buf: &n::Buffer,
        range: Range<u64>,
    ) -> Result<(*mut u8, Mapping), mapping::Error> {
        if (range.end - range.start) > buf.size_in_bytes as _ {
            return Err(mapping::Error::OutOfBounds);
        }

        let range = winapi::D3D12_RANGE {
            Begin: range.start,
            End: range.end,
        };
        let mut ptr = ptr::null_mut();
        assert_eq!(winapi::S_OK, unsafe {
            (*buf.resource).Map(0, &range, &mut ptr)
        });

        Ok((ptr as *mut _, Mapping {}))
    }

    fn unmap_mapping_raw(&mut self, _mapping: Mapping) {
        unimplemented!()
    }

    fn create_semaphore(&mut self) -> n::Semaphore {
        let fence = self.create_fence(false);
        n::Semaphore {
            raw: fence.raw,
        }
    }

    fn create_fence(&mut self, _signaled: bool) -> n::Fence {
        let mut handle = ptr::null_mut();
        assert_eq!(winapi::S_OK, unsafe {
            self.device.CreateFence(
                0,
                winapi::D3D12_FENCE_FLAGS(0),
                &dxguid::IID_ID3D12Fence,
                &mut handle,
            )
        });

        n::Fence {
            raw: unsafe { ComPtr::new(handle as *mut _) },
        }
    }

    fn reset_fences(&mut self, fences: &[&n::Fence]) {
        for fence in fences {
            assert_eq!(winapi::S_OK, unsafe {
                fence.raw.clone().Signal(0)
            });
        }
    }

    fn wait_for_fences(&mut self, fences: &[&n::Fence], wait: d::WaitFor, timeout_ms: u32) -> bool {
        for _ in self.events.len() .. fences.len() {
            self.events.push(unsafe {
                kernel32::CreateEventA(
                    ptr::null_mut(),
                    winapi::FALSE, winapi::FALSE,
                    ptr::null(),
                )
            });
        }

        for (&event, fence) in self.events.iter().zip(fences.iter()) {
            assert_eq!(winapi::S_OK, unsafe {
                kernel32::ResetEvent(event);
                fence.raw.clone().SetEventOnCompletion(1, event)
            });
        }

        let all = match wait {
            d::WaitFor::Any => winapi::FALSE,
            d::WaitFor::All => winapi::TRUE,
        };
        let hr = unsafe {
            kernel32::WaitForMultipleObjects(fences.len() as u32, self.events.as_ptr(), all, timeout_ms)
        };

        const WAIT_OBJECT_LAST: u32 = winapi::WAIT_OBJECT_0 + winapi::MAXIMUM_WAIT_OBJECTS;
        const WAIT_ABANDONED_LAST: u32 = winapi::WAIT_ABANDONED_0 + winapi::MAXIMUM_WAIT_OBJECTS;
        match hr {
            winapi::WAIT_OBJECT_0 ... WAIT_OBJECT_LAST => true,
            winapi::WAIT_ABANDONED_0 ... WAIT_ABANDONED_LAST => true, //TODO?
            winapi::WAIT_TIMEOUT => false,
            _ => panic!("Unexpected wait status 0x{:X}", hr),
        }
    }

    fn destroy_heap(&mut self, mut heap: n::Heap) {
        unsafe { (*heap.raw).Release(); }
    }

    fn destroy_shader_lib(&mut self, _shader_lib: n::ShaderLib) {
        unimplemented!()
    }

    fn destroy_renderpass(&mut self, _rp: n::RenderPass) {
        unimplemented!()
    }

    fn destroy_pipeline_layout(&mut self, _pl: n::PipelineLayout) {
        unimplemented!()
    }

    fn destroy_graphics_pipeline(&mut self, mut pipeline: n::GraphicsPipeline) {
        unsafe { (*pipeline.raw).Release(); }
    }

    fn destroy_compute_pipeline(&mut self, _pipeline: n::ComputePipeline) {
        unimplemented!()
    }

    fn destroy_framebuffer(&mut self, _fb: n::FrameBuffer) {
        unimplemented!()
    }

    fn destroy_buffer(&mut self, mut buffer: n::Buffer) {
        unsafe { (*buffer.resource).Release(); }
    }

    fn destroy_image(&mut self, mut image: n::Image) {
        unsafe { (*image.resource).Release(); }
    }

    fn destroy_render_target_view(&mut self, _rtv: n::RenderTargetView) {
        // Just drop
    }

    fn destroy_depth_stencil_view(&mut self, _dsv: n::DepthStencilView) {
        // Just drop
    }

    fn destroy_constant_buffer_view(&mut self, _: n::ConstantBufferView) {
        unimplemented!()
    }

    fn destroy_shader_resource_view(&mut self, _srv: n::ShaderResourceView) {
        // Just drop
    }

    fn destroy_unordered_access_view(&mut self, _uav: n::UnorderedAccessView) {
        unimplemented!()
    }

    fn destroy_sampler(&mut self, _sampler: n::Sampler) {
        // Just drop
    }

    fn destroy_descriptor_pool(&mut self, _pool: n::DescriptorPool) {
        // Just drop
    }

    fn destroy_descriptor_set_layout(&mut self, _layout: n::DescriptorSetLayout) {
        unimplemented!()
    }

    fn destroy_fence(&mut self, _fence: n::Fence) {
        // Just drop, ComPtr backed
    }

    fn destroy_semaphore(&mut self, _semaphore: n::Semaphore) {
        // Just drop, ComPtr backed
    }
}
