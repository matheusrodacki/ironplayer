//! Probe standalone do D3D11 Video Processor — isola a causa do E_INVALIDARG
//! no `VideoProcessorBlt` observado no perfil Performance (SPEC-AV-006).
//!
//! Testa combinações de: flags do device, bind flags da textura de entrada,
//! misc/bind flags da textura de saída, contagem de frames de referência,
//! output rate, rects, frame rates do content desc e slice do frame atual.
//!
//! Uso: `cargo run --release -p av --example vp_probe`

#[cfg(not(windows))]
fn main() {
    eprintln!("vp_probe só roda no Windows");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use windows::core::Interface;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    };

    const W: u32 = 1920;
    const H: u32 = 1080;
    const CODED_H: u32 = 1088;
    const ARRAY: u32 = 8;

    fn create_device(video_support: bool) -> (ID3D11Device, ID3D11DeviceContext) {
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if video_support {
            flags |= D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        }
        // Tenta com debug layer para mensagens de validação detalhadas.
        for try_debug in [true, false] {
            let f = if try_debug {
                flags | D3D11_CREATE_DEVICE_DEBUG
            } else {
                flags
            };
            let mut device = None;
            let mut context = None;
            let ok = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    f,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .is_ok();
            if !ok {
                continue;
            }
            let device = device.unwrap();
            let context = context.unwrap();
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = unsafe { mt.SetMultithreadProtected(true) };
            }
            if try_debug {
                println!("  [debug layer ATIVA]");
            }
            return (device, context);
        }
        panic!("D3D11CreateDevice falhou");
    }

    fn dump_info_queue(device: &ID3D11Device) {
        let Ok(queue) = device.cast::<ID3D11InfoQueue>() else {
            return;
        };
        unsafe {
            let n = queue.GetNumStoredMessages();
            for i in 0..n {
                let mut len = 0usize;
                if queue.GetMessage(i, None, &mut len).is_err() || len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; len];
                let msg = buf.as_mut_ptr() as *mut D3D11_MESSAGE;
                if queue.GetMessage(i, Some(msg), &mut len).is_ok() {
                    let m = &*msg;
                    let desc = std::slice::from_raw_parts(
                        m.pDescription as *const u8,
                        m.DescriptionByteLength.saturating_sub(1),
                    );
                    println!("  [dbg] {}", String::from_utf8_lossy(desc));
                }
            }
            queue.ClearStoredMessages();
        }
    }

    fn create_tex(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        array: u32,
        bind: u32,
        misc: u32,
    ) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: array,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: misc,
        };
        let mut tex = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| format!("CreateTexture2D: {e}"))?;
        }
        tex.ok_or_else(|| "textura nula".into())
    }

    struct Vp {
        video_device: ID3D11VideoDevice,
        video_context: ID3D11VideoContext,
        enumerator: ID3D11VideoProcessorEnumerator,
        processor: ID3D11VideoProcessor,
        past: u32,
        future: u32,
        rate_index: u32,
        caps: u32,
    }

    fn create_vp(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        content_rates: bool,
    ) -> Result<Vp, String> {
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|e| format!("cast VideoDevice: {e}"))?;
        let video_context: ID3D11VideoContext =
            context.cast().map_err(|e| format!("cast VideoContext: {e}"))?;
        let (in_rate, out_rate) = if content_rates {
            (
                DXGI_RATIONAL {
                    Numerator: 30000,
                    Denominator: 1001,
                },
                DXGI_RATIONAL {
                    Numerator: 30000,
                    Denominator: 1001,
                },
            )
        } else {
            (
                DXGI_RATIONAL {
                    Numerator: 0,
                    Denominator: 0,
                },
                DXGI_RATIONAL {
                    Numerator: 0,
                    Denominator: 0,
                },
            )
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_INTERLACED_TOP_FIELD_FIRST,
            InputFrameRate: in_rate,
            InputWidth: W,
            InputHeight: H,
            OutputFrameRate: out_rate,
            OutputWidth: W,
            OutputHeight: H,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| format!("CreateVideoProcessorEnumerator: {e}"))?
        };
        let mut vp_caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
        unsafe {
            enumerator
                .GetVideoProcessorCaps(&mut vp_caps)
                .map_err(|e| format!("GetVideoProcessorCaps: {e}"))?;
        }
        // Mesma seleção do d3d11_vp.rs: adaptive (0x4) > bob (0x2) > primeiro.
        let mut selected = None;
        for desired in [0x4u32, 0x2u32] {
            for n in 0..vp_caps.RateConversionCapsCount {
                let mut rc = D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default();
                if unsafe { enumerator.GetVideoProcessorRateConversionCaps(n, &mut rc) }.is_err() {
                    continue;
                }
                if (rc.ProcessorCaps & desired) == desired {
                    selected = Some((n, rc));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let (rate_index, rc) = selected.ok_or("nenhum rate conversion cap")?;
        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, rate_index)
                .map_err(|e| format!("CreateVideoProcessor: {e}"))?
        };
        // Estado do stream configurado em run_case conforme os knobs do Case.
        Ok(Vp {
            video_device,
            video_context,
            enumerator,
            processor,
            past: rc.PastFrames,
            future: rc.FutureFrames,
            rate_index,
            caps: rc.ProcessorCaps,
        })
    }

    fn input_view(
        vp: &Vp,
        tex: &ID3D11Texture2D,
        slice: u32,
    ) -> Result<ID3D11VideoProcessorInputView, String> {
        let res: ID3D11Resource = tex.cast().map_err(|e| format!("cast: {e}"))?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: slice,
                },
            },
        };
        let mut view = None;
        unsafe {
            vp.video_device
                .CreateVideoProcessorInputView(&res, &vp.enumerator, &desc, Some(&mut view))
                .map_err(|e| format!("CreateInputView: {e}"))?;
        }
        view.ok_or_else(|| "input view nula".into())
    }

    fn output_view(
        vp: &Vp,
        tex: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView, String> {
        let res: ID3D11Resource = tex.cast().map_err(|e| format!("cast: {e}"))?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            vp.video_device
                .CreateVideoProcessorOutputView(&res, &vp.enumerator, &desc, Some(&mut view))
                .map_err(|e| format!("CreateOutputView: {e}"))?;
        }
        view.ok_or_else(|| "output view nula".into())
    }

    #[allow(clippy::too_many_arguments)]
    fn blt(
        vp: &Vp,
        out_view: &ID3D11VideoProcessorOutputView,
        current: &ID3D11VideoProcessorInputView,
        past: &mut [Option<ID3D11VideoProcessorInputView>],
        future: &mut [Option<ID3D11VideoProcessorInputView>],
        input_frame_or_field: u32,
    ) -> Result<(), String> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: input_frame_or_field,
            PastFrames: past.len() as u32,
            FutureFrames: future.len() as u32,
            ppPastSurfaces: if past.is_empty() {
                std::ptr::null_mut()
            } else {
                past.as_mut_ptr()
            },
            pInputSurface: std::mem::ManuallyDrop::new(Some(current.clone())),
            ppFutureSurfaces: if future.is_empty() {
                std::ptr::null_mut()
            } else {
                future.as_mut_ptr()
            },
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        unsafe {
            vp.video_context
                .VideoProcessorBlt(&vp.processor, out_view, 0, std::slice::from_ref(&stream))
                .map_err(|e| format!("{e}"))
        }
    }

    struct Case {
        name: &'static str,
        video_support: bool,
        in_bind: u32,
        in_array: u32,
        in_coded_h: u32,
        out_bind: u32,
        out_misc: u32,
        with_refs: bool,
        /// Refs em slices distintos do array (true) ou todos no slice do atual (false).
        refs_distinct_slices: bool,
        /// Slice da view do frame atual.
        current_slice: u32,
        /// OUTPUT_RATE_HALF (true, app atual) ou NORMAL (false, estilo VLC).
        rate_half: bool,
        /// Chamar SetStreamAutoProcessingMode(false)?
        set_auto_off: bool,
        /// Setar StreamDestRect + OutputTargetRect (estilo mpv/VLC)?
        set_rects: bool,
        /// Valor de InputFrameOrField no Blt.
        input_frame_or_field: u32,
        /// Preencher InputFrameRate/OutputFrameRate no content desc (estilo VLC/GStreamer).
        content_rates: bool,
    }

    pub fn run() {
        let decoder_bind = D3D11_BIND_DECODER.0 as u32;
        let srv_rt = (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32;
        let shared_nt = (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32;

        let base = Case {
            name: "",
            video_support: false,
            in_bind: decoder_bind,
            in_array: ARRAY,
            in_coded_h: CODED_H,
            out_bind: srv_rt,
            out_misc: shared_nt,
            with_refs: true,
            refs_distinct_slices: true,
            current_slice: 0,
            rate_half: true,
            set_auto_off: true,
            set_rects: false,
            input_frame_or_field: 0,
            content_rates: false,
        };
        let cases = [
            Case {
                name: "G: baseline app (HALF, auto off, refs distintos)",
                ..base
            },
            Case {
                name: "R: OUTPUT_RATE_NORMAL com refs distintos",
                rate_half: false,
                ..base
            },
            Case {
                name: "S: sem SetStreamAutoProcessingMode(false)",
                set_auto_off: false,
                ..base
            },
            Case {
                name: "T: com DestRect + OutputTargetRect (estilo VLC)",
                set_rects: true,
                ..base
            },
            Case {
                name: "U: InputFrameOrField = 2",
                input_frame_or_field: 2,
                ..base
            },
            Case {
                name: "V: NORMAL + rects + auto off",
                rate_half: false,
                set_rects: true,
                ..base
            },
            Case {
                name: "W: content desc com frame rates 29.97 (refs distintos)",
                content_rates: true,
                ..base
            },
            Case {
                name: "X: rates + rects + NORMAL (setup VLC completo)",
                content_rates: true,
                set_rects: true,
                rate_half: false,
                ..base
            },
        ];

        for case in &cases {
            print!("{} → ", case.name);
            match run_case(case) {
                Ok(()) => println!("OK"),
                Err(e) => println!("FALHOU: {e}"),
            }
        }
    }

    fn run_case(case: &Case) -> Result<(), String> {
        let (device, context) = create_device(case.video_support);
        let vp = create_vp(&device, &context, case.content_rates)?;
        println!(
            "\n  [caps: rate_index={} caps={:#x} past={} future={}]",
            vp.rate_index, vp.caps, vp.past, vp.future
        );

        let in_tex = create_tex(
            &device,
            W,
            case.in_coded_h,
            case.in_array,
            case.in_bind,
            0,
        )?;
        let out_tex = create_tex(&device, W, H, 1, case.out_bind, case.out_misc)?;

        // Estado do stream conforme os knobs do caso.
        unsafe {
            let rc_rect = RECT {
                left: 0,
                top: 0,
                right: W as i32,
                bottom: H as i32,
            };
            vp.video_context.VideoProcessorSetStreamSourceRect(
                &vp.processor,
                0,
                true,
                Some(&rc_rect),
            );
            if case.set_rects {
                vp.video_context.VideoProcessorSetStreamDestRect(
                    &vp.processor,
                    0,
                    true,
                    Some(&rc_rect),
                );
                vp.video_context
                    .VideoProcessorSetOutputTargetRect(&vp.processor, true, Some(&rc_rect));
            }
            if case.set_auto_off {
                vp.video_context
                    .VideoProcessorSetStreamAutoProcessingMode(&vp.processor, 0, false);
            }
            vp.video_context.VideoProcessorSetStreamFrameFormat(
                &vp.processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_INTERLACED_TOP_FIELD_FIRST,
            );
            vp.video_context.VideoProcessorSetStreamOutputRate(
                &vp.processor,
                0,
                if case.rate_half {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_HALF
                } else {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL
                },
                false,
                None,
            );
        }

        let current = input_view(&vp, &in_tex, case.current_slice)?;
        let out_view = output_view(&vp, &out_tex)?;

        let (mut past, mut future) = if case.with_refs {
            let slice_for = |i: u32| {
                if case.refs_distinct_slices {
                    (case.current_slice + 1 + i) % case.in_array
                } else {
                    case.current_slice
                }
            };
            let past: Vec<_> = (0..vp.past)
                .map(|i| input_view(&vp, &in_tex, slice_for(i)).map(Some))
                .collect::<Result<_, _>>()?;
            let future: Vec<_> = (0..vp.future)
                .map(|i| input_view(&vp, &in_tex, slice_for(vp.past + i)).map(Some))
                .collect::<Result<_, _>>()?;
            (past, future)
        } else {
            (Vec::new(), Vec::new())
        };

        let r = blt(
            &vp,
            &out_view,
            &current,
            &mut past,
            &mut future,
            case.input_frame_or_field,
        );
        dump_info_queue(&device);
        r?;
        unsafe { context.Flush() };
        Ok(())
    }
}
