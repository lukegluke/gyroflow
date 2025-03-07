// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright © 2021-2022 Adrian <adrian.eddy at gmail>

#[cfg(feature = "use-opencl")]
pub mod opencl;
pub mod wgpu;

pub struct BufferDescription<'a> {
    pub input_size:  (usize, usize, usize), // width, height, stride
    pub output_size: (usize, usize, usize), // width, height, stride

    pub input_rect:  Option<(usize, usize, usize, usize)>, // x, y, width, height
    pub output_rect: Option<(usize, usize, usize, usize)>, // x, y, width, height

    pub buffers: BufferSource<'a>
}

pub enum BufferSource<'a> {
    Cpu {
        input: &'a mut [u8],
        output: &'a mut [u8]
    },
    #[cfg(feature = "use-opencl")]
    OpenCL {
        input: ocl::ffi::cl_mem,
        output: ocl::ffi::cl_mem,
        queue: ocl::ffi::cl_command_queue
    },
    /*OpenGL {
        input: u32, // GLuint
        output: u32, // GLuint
    },
    DirectX {
        input: u32,
        output: u32,
    },
    Cuda {
        input: u32,
        output: u32,
    },
    Metal {
        input: u32,
        output: u32,
    },
    Vulkan {
        input: u32,
        output: u32,
    }*/
}

pub fn initialize_contexts() -> Option<(String, String)> {
    #[cfg(feature = "use-opencl")]
    if std::env::var("NO_OPENCL").unwrap_or_default().is_empty() {
        let cl = std::panic::catch_unwind(|| {
            opencl::OclWrapper::initialize_context()
        });
        match cl {
            Ok(Ok(names)) => { return Some(names); },
            Ok(Err(e)) => { log::error!("OpenCL error: {:?}", e); },
            Err(e) => {
                if let Some(s) = e.downcast_ref::<&str>() {
                    log::error!("Failed to initialize OpenCL {}", s);
                } else if let Some(s) = e.downcast_ref::<String>() {
                    log::error!("Failed to initialize OpenCL {}", s);
                } else {
                    log::error!("Failed to initialize OpenCL {:?}", e);
                }
            }
        }
    }

    if std::env::var("NO_WGPU").unwrap_or_default().is_empty() {
        let wgpu = std::panic::catch_unwind(|| {
            wgpu::WgpuWrapper::initialize_context()
        });
        match wgpu {
            Ok(Some(names)) => { return Some(names); },
            Ok(None) => { log::error!("wgpu init error"); },
            Err(e) => {
                if let Some(s) = e.downcast_ref::<&str>() {
                    log::error!("Failed to initialize wgpu {}", s);
                } else if let Some(s) = e.downcast_ref::<String>() {
                    log::error!("Failed to initialize wgpu {}", s);
                } else {
                    log::error!("Failed to initialize wgpu {:?}", e);
                }
            }
        }
    }

    None
}
