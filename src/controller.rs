// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright © 2021-2022 Adrian <adrian.eddy at gmail>

use itertools::{Either, Itertools};
use qmetaobject::*;
use nalgebra::Vector4;
use std::sync::Arc;
use std::cell::RefCell;
use std::sync::atomic::{ AtomicBool, AtomicUsize, Ordering::SeqCst };
use std::collections::BTreeSet;
use std::str::FromStr;

use qml_video_rs::video_item::MDKVideoItem;

use crate::core;
use crate::core::StabilizationManager;
#[cfg(feature = "opencv")]
use crate::core::calibration::LensCalibrator;
use crate::core::synchronization::AutosyncProcess;
use crate::core::stabilization;
use crate::core::synchronization;
use crate::core::keyframes::*;
use crate::rendering;
use crate::util;
use crate::wrap_simple_method;
use crate::rendering::VideoProcessor;
use crate::ui::components::TimelineGyroChart::TimelineGyroChart;
use crate::ui::components::TimelineKeyframesView::TimelineKeyframesView;
use crate::ui::components::FrequencyGraph::FrequencyGraph;
use crate::qt_gpu::qrhi_undistort;

#[derive(Default, SimpleListItem)]
struct OffsetItem {
    pub timestamp_us: i64,
    pub offset_ms: f64,
}

#[derive(Default, SimpleListItem)]
struct CalibrationItem {
    pub timestamp_us: i64,
    pub sharpness: f64,
    pub is_forced: bool,
}

#[derive(Default, QObject)]
pub struct Controller {
    base: qt_base_class!(trait QObject),

    init_player: qt_method!(fn(&self, player: QJSValue)),
    reset_player: qt_method!(fn(&self, player: QJSValue)),
    load_video: qt_method!(fn(&self, url: QUrl, player: QJSValue)),
    video_file_loaded: qt_method!(fn(&self, url: QUrl, player: QJSValue)),
    load_telemetry: qt_method!(fn(&self, url: QUrl, is_video: bool, player: QJSValue, chart: QJSValue, kfview: QJSValue)),
    load_lens_profile: qt_method!(fn(&mut self, path: String)),
    load_lens_profile_url: qt_method!(fn(&mut self, url: QUrl)),
    export_lens_profile: qt_method!(fn(&mut self, url: QUrl, info: QJsonObject, upload: bool)),
    export_lens_profile_filename: qt_method!(fn(&mut self, info: QJsonObject) -> QString),

    set_of_method: qt_method!(fn(&self, v: u32)),
    start_autosync: qt_method!(fn(&mut self, timestamps_fract: String, sync_params: String, mode: String)),
    update_chart: qt_method!(fn(&self, chart: QJSValue)),
    update_frequency_graph: qt_method!(fn(&self, graph: QJSValue, idx: usize, ts: f64, sr: f64, fft_size: usize)),
    update_keyframes_view: qt_method!(fn(&self, kfview: QJSValue)),
    rolling_shutter_estimated: qt_signal!(rolling_shutter: f64),
    estimate_bias: qt_method!(fn(&self, timestamp_fract: QString)),
    bias_estimated: qt_signal!(bx: f64, by: f64, bz: f64),
    orientation_guessed: qt_signal!(orientation: QString),
    get_optimal_sync_points: qt_method!(fn(&mut self, target_sync_points: usize) -> QString),

    start_autocalibrate: qt_method!(fn(&self, max_points: usize, every_nth_frame: usize, iterations: usize, max_sharpness: f64, custom_timestamp_ms: f64, no_marker: bool)),

    telemetry_loaded: qt_signal!(is_main_video: bool, filename: QString, camera: QString, imu_orientation: QString, contains_gyro: bool, contains_raw_gyro: bool, contains_quats: bool, frame_readout_time: f64, camera_id_json: QString, sample_rate: f64),
    lens_profile_loaded: qt_signal!(lens_json: QString, filepath: QString),
    realtime_fps_loaded: qt_signal!(fps: f64),

    set_smoothing_method: qt_method!(fn(&self, index: usize) -> QJsonArray),
    get_smoothing_max_angles: qt_method!(fn(&self) -> QJsonArray),
    get_smoothing_status: qt_method!(fn(&self) -> QJsonArray),
    set_smoothing_param: qt_method!(fn(&self, name: QString, val: f64)),
    set_horizon_lock: qt_method!(fn(&self, lock_percent: f64, roll: f64)),
    set_use_gravity_vectors: qt_method!(fn(&self, v: bool)),
    set_preview_resolution: qt_method!(fn(&mut self, target_height: i32, player: QJSValue)),
    set_background_color: qt_method!(fn(&self, color: QString, player: QJSValue)),
    set_integration_method: qt_method!(fn(&self, index: usize)),

    set_offset: qt_method!(fn(&self, timestamp_us: i64, offset_ms: f64)),
    remove_offset: qt_method!(fn(&self, timestamp_us: i64)),
    clear_offsets: qt_method!(fn(&self)),
    offset_at_video_timestamp: qt_method!(fn(&self, timestamp_us: i64) -> f64),
    offsets_model: qt_property!(RefCell<SimpleListModel<OffsetItem>>; NOTIFY offsets_updated),
    offsets_updated: qt_signal!(),

    load_profiles: qt_method!(fn(&self, reload_from_disk: bool)),
    all_profiles_loaded: qt_signal!(profiles: QVariantList),
    fetch_profiles_from_github: qt_method!(fn(&self)),
    lens_profiles_updated: qt_signal!(reload_from_disk: bool),

    set_sync_lpf: qt_method!(fn(&self, lpf: f64)),
    set_imu_lpf: qt_method!(fn(&self, lpf: f64)),
    set_imu_rotation: qt_method!(fn(&self, pitch_deg: f64, roll_deg: f64, yaw_deg: f64)),
    set_acc_rotation: qt_method!(fn(&self, pitch_deg: f64, roll_deg: f64, yaw_deg: f64)),
    set_imu_orientation: qt_method!(fn(&self, orientation: String)),
    set_imu_bias: qt_method!(fn(&self, bx: f64, by: f64, bz: f64)),
    recompute_gyro: qt_method!(fn(&self)),

    override_video_fps: qt_method!(fn(&self, fps: f64)),
    get_org_duration_ms: qt_method!(fn(&self) -> f64),
    get_scaled_duration_ms: qt_method!(fn(&self) -> f64),
    get_scaled_fps: qt_method!(fn(&self) -> f64),

    recompute_threaded: qt_method!(fn(&mut self)),
    request_recompute: qt_signal!(),

    stab_enabled: qt_property!(bool; WRITE set_stab_enabled),
    show_detected_features: qt_property!(bool; WRITE set_show_detected_features),
    show_optical_flow: qt_property!(bool; WRITE set_show_optical_flow),
    fov: qt_property!(f64; WRITE set_fov),
    frame_readout_time: qt_property!(f64; WRITE set_frame_readout_time),

    adaptive_zoom: qt_property!(f64; WRITE set_adaptive_zoom),
    zooming_center_x: qt_property!(f64; WRITE set_zooming_center_x),
    zooming_center_y: qt_property!(f64; WRITE set_zooming_center_y),

    lens_correction_amount: qt_property!(f64; WRITE set_lens_correction_amount),
    set_video_speed: qt_method!(fn(&self, v: f64, s: bool, z: bool)),

    input_horizontal_stretch: qt_property!(f64; WRITE set_input_horizontal_stretch),
    input_vertical_stretch: qt_property!(f64; WRITE set_input_vertical_stretch),
    lens_is_asymmetrical: qt_property!(bool; WRITE set_lens_is_asymmetrical),

    background_mode: qt_property!(i32; WRITE set_background_mode),
    background_margin: qt_property!(f64; WRITE set_background_margin),
    background_margin_feather: qt_property!(f64; WRITE set_background_margin_feather),

    lens_loaded: qt_property!(bool; NOTIFY lens_changed),
    set_lens_param: qt_method!(fn(&self, param: QString, value: f64)),
    lens_changed: qt_signal!(),

    gyro_loaded: qt_property!(bool; NOTIFY gyro_changed),
    gyro_changed: qt_signal!(),

    has_gravity_vectors: qt_property!(bool; READ has_gravity_vectors NOTIFY gyro_changed),

    compute_progress: qt_signal!(id: u64, progress: f64),
    sync_progress: qt_signal!(progress: f64, ready: usize, total: usize),

    set_video_rotation: qt_method!(fn(&self, angle: f64)),

    set_trim_start: qt_method!(fn(&self, trim_start: f64)),
    set_trim_end: qt_method!(fn(&self, trim_end: f64)),

    set_output_size: qt_method!(fn(&self, width: usize, height: usize)),

    chart_data_changed: qt_signal!(),
    keyframes_changed: qt_signal!(),

    cancel_current_operation: qt_method!(fn(&mut self)),

    sync_in_progress: qt_property!(bool; NOTIFY sync_in_progress_changed),
    sync_in_progress_changed: qt_signal!(),

    calib_in_progress: qt_property!(bool; NOTIFY calib_in_progress_changed),
    calib_in_progress_changed: qt_signal!(),
    calib_progress: qt_signal!(progress: f64, rms: f64, ready: usize, total: usize, good: usize),

    loading_gyro_in_progress: qt_property!(bool; NOTIFY loading_gyro_in_progress_changed),
    loading_gyro_in_progress_changed: qt_signal!(),
    loading_gyro_progress: qt_signal!(progress: f64),

    calib_model: qt_property!(RefCell<SimpleListModel<CalibrationItem>>; NOTIFY calib_model_updated),
    calib_model_updated: qt_signal!(),

    add_calibration_point: qt_method!(fn(&mut self, timestamp_us: i64, no_marker: bool)),
    remove_calibration_point: qt_method!(fn(&mut self, timestamp_us: i64)),

    get_current_fov: qt_method!(fn(&self) -> f64),
    quats_at_timestamp: qt_method!(fn(&self, timestamp_us: i64) -> QVariantList),
    get_scaling_ratio: qt_method!(fn(&self) -> f64),
    get_min_fov: qt_method!(fn(&self) -> f64),

    init_calibrator: qt_method!(fn(&mut self)),

    get_paths_from_gyroflow_file: qt_method!(fn(&mut self, url: QUrl) -> QStringList),
    import_gyroflow_file: qt_method!(fn(&mut self, url: QUrl)),
    import_gyroflow_data: qt_method!(fn(&mut self, data: QString)),
    gyroflow_file_loaded: qt_signal!(obj: QJsonObject),
    export_gyroflow_file: qt_method!(fn(&self, thin: bool, extended: bool, additional_data: QJsonObject, override_location: QString, overwrite: bool)),
    export_gyroflow_data: qt_method!(fn(&self, thin: bool, extended: bool, additional_data: QJsonObject) -> QString),

    check_updates: qt_method!(fn(&self)),
    updates_available: qt_signal!(version: QString, changelog: QString),
    rate_profile: qt_method!(fn(&self, name: QString, json: QString, is_good: bool)),
    request_profile_ratings: qt_method!(fn(&self)),

    set_zero_copy: qt_method!(fn(&self, player: QJSValue, enabled: bool)),
    set_gpu_decoding: qt_method!(fn(&self, enabled: bool)),

    list_gpu_devices: qt_method!(fn(&self)),
    set_device: qt_method!(fn(&self, i: i32)),
    set_rendering_gpu_type_from_name: qt_method!(fn(&self, name: String)),
    gpu_list_loaded: qt_signal!(list: QJsonArray),

    is_superview: qt_property!(bool; WRITE set_is_superview),

    file_exists: qt_method!(fn(&self, path: QString) -> bool),
    file_size: qt_method!(fn(&self, path: QString) -> u64),
    video_duration: qt_method!(fn(&self, path: QString) -> f64),
    resolve_android_url: qt_method!(fn(&self, url: QString) -> QString),
    open_file_externally: qt_method!(fn(&self, path: QString)),
    get_username: qt_method!(fn(&self) -> QString),
    clear_settings: qt_method!(fn(&self)),

    url_to_path: qt_method!(fn(&self, url: QUrl) -> QString),
    path_to_url: qt_method!(fn(&self, path: QString) -> QUrl),

    image_to_b64: qt_method!(fn(&self, img: QImage) -> QString),
    export_preset: qt_method!(fn(&self, url: QUrl, data: QJsonObject)),

    message: qt_signal!(text: QString, arg: QString, callback: QString),
    error: qt_signal!(text: QString, arg: QString, callback: QString),

    gyroflow_exists: qt_signal!(path: QString, thin: bool, extended: bool),
    request_location: qt_signal!(path: QString, thin: bool, extended: bool),

    set_keyframe: qt_method!(fn(&self, typ: String, timestamp_us: i64, value: f64)),
    set_keyframe_easing: qt_method!(fn(&self, typ: String, timestamp_us: i64, easing: String)),
    keyframe_easing: qt_method!(fn(&self, typ: String, timestamp_us: i64) -> String),
    remove_keyframe: qt_method!(fn(&self, typ: String, timestamp_us: i64)),
    clear_keyframes_type: qt_method!(fn(&self, typ: String)),
    keyframe_value_at_video_timestamp: qt_method!(fn(&self, typ: String, timestamp_ms: f64) -> QJSValue),
    is_keyframed: qt_method!(fn(&self, typ: String) -> bool),

    keyframe_value_updated: qt_signal!(keyframe: String, value: f64),
    update_keyframe_values: qt_method!(fn(&self, timestamp_ms: f64)),

    check_external_sdk: qt_method!(fn(&self, path: QString) -> bool),
    install_external_sdk: qt_method!(fn(&self, path: QString)),
    external_sdk_progress: qt_signal!(percent: f64, sdk_name: QString, error_string: QString, path: QString),

    mp4_merge: qt_method!(fn(&self, file_list: QStringList)),
    mp4_merge_progress: qt_signal!(percent: f64, error_string: QString, path: QString),

    image_sequence_start: qt_property!(i32),
    image_sequence_fps: qt_property!(f64),

    preview_resolution: i32,

    cancel_flag: Arc<AtomicBool>,

    ongoing_computations: BTreeSet<u64>,

    pub stabilizer: Arc<StabilizationManager<stabilization::RGBA8>>,
}

impl Controller {
    pub fn new() -> Self {
        Self {
            preview_resolution: 720,
            ..Default::default()
        }
    }

    fn load_video(&mut self, url: QUrl, player: QJSValue) {
        self.stabilizer.clear();
        self.chart_data_changed();
        self.keyframes_changed();
        self.update_offset_model();
        *self.stabilizer.input_file.write() = gyroflow_core::InputFile {
            path: util::url_to_path(url.clone()),
            image_sequence_start: self.image_sequence_start,
            image_sequence_fps: self.image_sequence_fps
        };

        let mut custom_decoder = QString::default(); // eg. BRAW:format=rgba64le
        if self.image_sequence_start > 0 {
            custom_decoder = QString::from(format!("FFmpeg:avformat_options=start_number={}", self.image_sequence_start));
        }

        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()
            vid.setUrl(url, custom_decoder);
        }
    }

    fn start_autosync(&mut self, timestamps_fract: String, sync_params: String, mode: String) {
        rendering::clear_log();

        let sync_params = serde_json::from_str(&sync_params) as serde_json::Result<synchronization::SyncParams>;
        if let Err(e) = sync_params {
            self.sync_in_progress = false;
            self.sync_in_progress_changed();
            return self.error(QString::from("An error occured: %1"), QString::from(format!("JSON parse error: {}", e)), QString::default());
        }
        let mut sync_params = sync_params.unwrap();

        sync_params.initial_offset     *= 1000.0; // s to ms
        sync_params.time_per_syncpoint *= 1000.0; // s to ms
        sync_params.search_size        *= 1000.0; // s to ms
        sync_params.every_nth_frame     = sync_params.every_nth_frame.max(1);

        let for_rs = mode == "estimate_rolling_shutter";

        let every_nth_frame = sync_params.every_nth_frame;

        self.sync_in_progress = true;
        self.sync_in_progress_changed();

        let size = self.stabilizer.params.read().size;

        let timestamps_fract: Vec<f64> = timestamps_fract.split(';').filter_map(|x| x.parse::<f64>().ok()).collect();

        let progress = util::qt_queued_callback_mut(self, |this, (percent, ready, total): (f64, usize, usize)| {
            this.sync_in_progress = ready < total || percent < 1.0;
            this.sync_in_progress_changed();
            this.chart_data_changed();
            this.sync_progress(percent, ready, total);
        });
        let set_offsets = util::qt_queued_callback_mut(self, move |this, offsets: Vec<(f64, f64, f64)>| {
            if for_rs {
                if let Some(offs) = offsets.first() {
                    this.rolling_shutter_estimated(offs.1);
                }
            } else {
                let mut gyro = this.stabilizer.gyro.write();
                for x in offsets {
                    ::log::info!("Setting offset at {:.4}: {:.4} (cost {:.4})", x.0, x.1, x.2);
                    let new_ts = ((x.0 - x.1) * 1000.0) as i64;
                    // Remove existing offsets within 100ms range
                    gyro.remove_offsets_near(new_ts, 100.0);
                    gyro.set_offset(new_ts, x.1);
                }
                this.stabilizer.keyframes.write().update_gyro(&gyro);
                this.stabilizer.invalidate_zooming();
            }
            this.update_offset_model();
            this.request_recompute();
        });
        let set_orientation = util::qt_queued_callback_mut(self, move |this, orientation: String| {
            ::log::info!("Setting orientation {}", &orientation);
            this.orientation_guessed(QString::from(orientation));
        });
        let err = util::qt_queued_callback_mut(self, |this, (msg, mut arg): (String, String)| {
            arg.push_str("\n\n");
            arg.push_str(&rendering::get_log());

            this.error(QString::from(msg), QString::from(arg), QString::default());

            this.sync_in_progress = false;
            this.sync_in_progress_changed();
            this.update_offset_model();
            this.request_recompute();
        });
        self.sync_progress(0.0, 0, 0);

        self.cancel_flag.store(false, SeqCst);

        if let Ok(mut sync) = AutosyncProcess::from_manager(&self.stabilizer, &timestamps_fract, sync_params, mode, self.cancel_flag.clone()) {
            sync.on_progress(move |percent, ready, total| {
                progress((percent, ready, total));
            });
            sync.on_finished(move |arg| {
                match arg {
                    Either::Left(offsets) => set_offsets(offsets),
                    Either::Right(Some(orientation)) => set_orientation(orientation.0),
                    _=> ()
                };
            });

            let ranges = sync.get_ranges();
            let cancel_flag = self.cancel_flag.clone();

            let input_file = self.stabilizer.input_file.read().clone();
            let (sw, sh) = (size.0 as u32, size.1 as u32);
            core::run_threaded(move || {
                let gpu_decoding = *rendering::GPU_DECODING.read();

                let mut frame_no = 0;
                let mut abs_frame_no = 0;

                let mut decoder_options = ffmpeg_next::Dictionary::new();
                if input_file.image_sequence_fps > 0.0 {
                    let fps = rendering::fps_to_rational(input_file.image_sequence_fps);
                    decoder_options.set("framerate", &format!("{}/{}", fps.numerator(), fps.denominator()));
                }
                if input_file.image_sequence_start > 0 {
                    decoder_options.set("start_number", &format!("{}", input_file.image_sequence_start));
                }

                let sync = std::rc::Rc::new(sync);

                match VideoProcessor::from_file(&input_file.path, gpu_decoding, 0, Some(decoder_options)) {
                    Ok(mut proc) => {
                        let err2 = err.clone();
                        let sync2 = sync.clone();
                        proc.on_frame(move |timestamp_us, input_frame, _output_frame, converter, _rate_control| {
                            assert!(_output_frame.is_none());

                            if abs_frame_no % every_nth_frame == 0 {
                                match converter.scale(input_frame, ffmpeg_next::format::Pixel::GRAY8, sw, sh) {
                                    Ok(small_frame) => {
                                        let (width, height, stride, pixels) = (small_frame.plane_width(0), small_frame.plane_height(0), small_frame.stride(0), small_frame.data(0));

                                        sync2.feed_frame(timestamp_us, frame_no, width, height, stride, pixels);
                                    },
                                    Err(e) => {
                                        err2(("An error occured: %1".to_string(), e.to_string()))
                                    }
                                }
                                frame_no += 1;
                            }
                            abs_frame_no += 1;
                            Ok(())
                        });
                        if let Err(e) = proc.start_decoder_only(ranges, cancel_flag.clone()) {
                            err(("An error occured: %1".to_string(), e.to_string()));
                        }
                        sync.finished_feeding_frames();
                    }
                    Err(error) => {
                        err(("An error occured: %1".to_string(), error.to_string()));
                    }
                }
            });
        } else {
            err(("An error occured: %1".to_string(), "Invalid parameters".to_string()));
        }
    }

    fn estimate_bias(&mut self, timestamps_fract: QString) {
        let timestamps_fract: Vec<f64> = timestamps_fract.to_string().split(';').filter_map(|x| x.parse::<f64>().ok()).collect();

        let org_duration_ms = self.stabilizer.params.read().duration_ms;

        // sample 400 ms
        let ranges_ms: Vec<(f64, f64)> = timestamps_fract.iter().map(|x| {
            let range = (
                ((x * org_duration_ms) - (200.0)).max(0.0),
                ((x * org_duration_ms) + (200.0)).min(org_duration_ms)
            );
            (range.0, range.1)
        }).collect();

        if !ranges_ms.is_empty() {
            let bias = self.stabilizer.gyro.read().find_bias(ranges_ms[0].0, ranges_ms[0].1);
            self.bias_estimated(bias.0, bias.1, bias.2);
        }
    }

    fn get_optimal_sync_points(&mut self, target_sync_points: usize) -> QString {
        let dur_ms = self.stabilizer.params.read().get_scaled_duration_ms();
        let trim_start = self.stabilizer.params.read().trim_start * dur_ms / 1000.0;
        let trim_end = self.stabilizer.params.read().trim_end * dur_ms / 1000.0;
        if let Some(mut optsync) = core::synchronization::optimsync::OptimSync::new(&self.stabilizer.gyro.read()) {
            let s: String = optsync.run(target_sync_points, trim_start, trim_end).iter().map(|x| x / dur_ms).map(|x| x.to_string()).join(";").chars().collect();
            QString::from(s)
        } else {
            QString::default()
        }
    }

    fn update_chart(&mut self, chart: QJSValue) {
        if let Some(chart) = chart.to_qobject::<TimelineGyroChart>() {
            let chart = unsafe { &mut *chart.as_ptr() }; // _self.borrow_mut();

            chart.setSyncResults(&*self.stabilizer.pose_estimator.estimated_gyro.read());
            chart.setSyncResultsQuats(&*self.stabilizer.pose_estimator.estimated_quats.read());

            chart.setFromGyroSource(&self.stabilizer.gyro.read());
        }
    }

    fn update_frequency_graph(&mut self, graph: QJSValue, idx: usize, ts: f64, sr: f64, fft_size: usize) {
        if let Some(graph) = graph.to_qobject::<FrequencyGraph>() {
            let graph = unsafe { &mut *graph.as_ptr() }; // _self.borrow_mut();
            
            let gyro = &self.stabilizer.gyro.read();
            let raw_imu = &gyro.raw_imu;
            
            if !raw_imu.is_empty() {
                let dt_ms = 1000.0 / sr;
                let center_ts = ts - gyro.offset_at_video_timestamp(ts);
                let last_ts  = center_ts + dt_ms * (fft_size as f64)/2.0;
                let mut sample_ts = last_ts.min(raw_imu.last().unwrap().timestamp_ms) - (fft_size as f64) * dt_ms;
                sample_ts = sample_ts.max(0.0);

                let mut prev_ts = 0.0;
                let mut prev_val = 0.0;

                let mut samples: Vec<f64> = Vec::with_capacity(fft_size);
                for x in raw_imu {
                    let mut val = 0.0;
                    if idx < 3 {
                        if let Some(g) = x.gyro.as_ref() {
                            val = g[idx % 3];
                        }
                    } else {
                        if let Some(g) = x.accl.as_ref() {
                            val = g[idx % 3];
                        }
                    }

                    while x.timestamp_ms > sample_ts && samples.len() < fft_size {
                        let frac = (sample_ts - prev_ts) / (x.timestamp_ms - prev_ts);
                        let interpolated = prev_val + (val - prev_val) * frac.clamp(0.0, 1.0);
                        samples.push(interpolated /*+ samples.last().unwrap_or(&0.0)*/);
                        sample_ts += dt_ms;
                    }

                    if samples.len() >= fft_size {
                        break;
                    }

                    prev_ts = x.timestamp_ms;
                    prev_val = val;
                }

                if samples.len() == fft_size {
                    graph.setData(&samples, sr);
                } else {
                    graph.setData(&[], 0.0);
                }
            }
        }
    }

    fn update_keyframes_view(&mut self, view: QJSValue) {
        if let Some(view) = view.to_qobject::<TimelineKeyframesView>() {
            let view = unsafe { &mut *view.as_ptr() }; // _self.borrow_mut();

            view.setKeyframes(&*self.stabilizer.keyframes.read());
        }
    }

    fn update_offset_model(&mut self) {
        self.offsets_model = RefCell::new(self.stabilizer.gyro.read().get_offsets().iter().map(|(k, v)| OffsetItem {
            timestamp_us: *k,
            offset_ms: *v
        }).collect());

        util::qt_queued_callback(self, |this, _| {
            this.offsets_updated();
            this.chart_data_changed();
        })(());
    }

    fn video_file_loaded(&mut self, url: QUrl, player: QJSValue) {
        let s = util::url_to_path(url);
        let stab = self.stabilizer.clone();

        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()
            let duration_ms = vid.duration;
            let fps = vid.frameRate;
            let frame_count = vid.frameCount as usize;
            let video_size = (vid.videoWidth as usize, vid.videoHeight as usize);

            self.set_preview_resolution(self.preview_resolution, player);

            if duration_ms > 0.0 && fps > 0.0 {
                if let Ok(_) = stab.init_from_video_data(&s, duration_ms, fps, frame_count, video_size) {
                    stab.set_output_size(video_size.0, video_size.1);
                }
            }
        }
    }

    fn load_telemetry(&mut self, url: QUrl, is_main_video: bool, player: QJSValue, chart: QJSValue, kfview: QJSValue) {
        let s = util::url_to_path(url);
        let stab = self.stabilizer.clone();
        let filename = QString::from(s.split('/').last().unwrap_or_default());
        self.loading_gyro_in_progress = true;
        self.loading_gyro_in_progress_changed();

        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()
            let duration_ms = vid.duration;
            let fps = vid.frameRate;
            let frame_count = vid.frameCount as usize;
            let video_size = (vid.videoWidth as usize, vid.videoHeight as usize);
            self.cancel_flag.store(false, SeqCst);
            let cancel_flag = self.cancel_flag.clone();

            if is_main_video {
                self.set_preview_resolution(self.preview_resolution, player);
            }

            let err = util::qt_queued_callback_mut(self, |this, (msg, arg): (String, String)| {
                this.error(QString::from(msg), QString::from(arg), QString::default());
            });

            let progress = util::qt_queued_callback_mut(self, move |this, progress: f64| {
                this.loading_gyro_in_progress = progress < 1.0;
                this.loading_gyro_progress(progress);
                this.loading_gyro_in_progress_changed();
            });
            let stab2 = stab.clone();
            let finished = util::qt_queued_callback_mut(self, move |this, params: (bool, QString, QString, QString, bool, bool, bool, f64, QString, f64)| {
                this.gyro_loaded = params.4; // Contains gyro
                this.gyro_changed();

                this.loading_gyro_in_progress = false;
                this.loading_gyro_progress(1.0);
                this.loading_gyro_in_progress_changed();

                this.update_offset_model();
                this.chart_data_changed();
                this.telemetry_loaded(params.0, params.1, params.2, params.3, params.4, params.5, params.6, params.7, params.8, params.9);

                stab2.invalidate_ongoing_computations();
                stab2.invalidate_smoothing();
                this.request_recompute();
            });
            let load_lens = util::qt_queued_callback_mut(self, move |this, path: String| {
                this.load_lens_profile(path);
            });
            let reload_lens = util::qt_queued_callback_mut(self, move |this, _| {
                let lens = this.stabilizer.lens.read();
                if this.lens_loaded || !lens.filename.is_empty() {
                    this.lens_loaded = true;
                    this.lens_changed();
                    let json = lens.get_json().unwrap_or_default();
                    this.lens_profile_loaded(QString::from(json), QString::from(lens.filename.as_str()));
                }
            });
            let on_metadata = util::qt_queued_callback_mut(self, move |this, md: core::gyro_source::FileMetadata| {
                if let Some(md_fps) = md.frame_rate {
                    let fps = this.stabilizer.params.read().fps;
                    if (md_fps - fps).abs() > 1.0 {
                        this.realtime_fps_loaded(md_fps);
                    }
                }
            });

            if duration_ms > 0.0 && fps > 0.0 {
                core::run_threaded(move || {
                    let mut file_metadata = None;
                    if is_main_video {
                        if let Err(e) = stab.init_from_video_data(&s, duration_ms, fps, frame_count, video_size) {
                            err(("An error occured: %1".to_string(), e.to_string()));
                        } else {
                            // Ignore the error here, video file may not contain the telemetry and it's ok
                            if let Ok(md) = stab.load_gyro_data(&s, progress, cancel_flag) {
                                file_metadata = Some(md);
                            }

                            if stab.set_output_size(video_size.0, video_size.1) {
                                stab.recompute_undistortion();
                            }
                        }
                    } else {
                        match stab.load_gyro_data(&s, progress, cancel_flag) {
                            Ok(md) => {
                                file_metadata = Some(md);
                            },
                            Err(e) => {
                                err(("An error occured: %1".to_string(), e.to_string()));
                            }
                        }
                    }
                    stab.recompute_smoothness();

                    let gyro = stab.gyro.read();
                    let detected = gyro.detected_source.as_ref().map(String::clone).unwrap_or_default();
                    let orientation = gyro.imu_orientation.as_ref().map(String::clone).unwrap_or_else(|| "XYZ".into());
                    let has_raw_gyro = !gyro.org_raw_imu.is_empty();
                    let has_quats = !gyro.org_quaternions.is_empty();
                    let has_gyro = has_raw_gyro || has_quats;
                    let sample_rate = gyro.get_sample_rate();
                    drop(gyro);

                    if let Some(chart) = chart.to_qobject::<TimelineGyroChart>() {
                        let chart = unsafe { &mut *chart.as_ptr() }; // _self.borrow_mut();
                        chart.setDurationMs(duration_ms);
                    }
                    if let Some(kfview) = kfview.to_qobject::<TimelineKeyframesView>() {
                        let kfview = unsafe { &mut *kfview.as_ptr() }; // _self.borrow_mut();
                        kfview.setDurationMs(duration_ms);
                    }
                    let camera_id = stab.camera_id.read();

                    let id_str = camera_id.as_ref().map(|v| v.identifier.clone()).unwrap_or_default();
                    if is_main_video && !id_str.is_empty() {
                        let db = stab.lens_profile_db.read();
                        if db.contains_id(&id_str) {
                            load_lens(id_str);
                        }
                    }
                    reload_lens(());
                    if let Some(md) = file_metadata {
                        on_metadata(md);
                    }

                    let frame_readout_time = stab.params.read().frame_readout_time;
                    let camera_id = camera_id.as_ref().map(|v| v.to_json()).unwrap_or_default();

                    finished((is_main_video, filename, QString::from(detected.trim()), QString::from(orientation), has_gyro, has_raw_gyro, has_quats, frame_readout_time, QString::from(camera_id), sample_rate));
                });
            }
        }
    }
    fn load_lens_profile_url(&mut self, url: QUrl) {
        self.load_lens_profile(util::url_to_path(url))
    }
    fn load_lens_profile(&mut self, path: String) {
        let (json, filepath) = {
            if let Err(e) = self.stabilizer.load_lens_profile(&path) {
                self.error(QString::from("An error occured: %1"), QString::from(e.to_string()), QString::default());
            }
            let lens = self.stabilizer.lens.read();
            (lens.get_json().unwrap_or_default(), lens.filename.clone())
        };
        self.lens_loaded = true;
        self.lens_changed();
        self.lens_profile_loaded(QString::from(json), QString::from(filepath));
        self.request_recompute();
    }

    fn set_preview_resolution(&mut self, target_height: i32, player: QJSValue) {
        self.preview_resolution = target_height;
        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()

            // fn aligned_to_8(mut x: u32) -> u32 { if x % 8 != 0 { x += 8 - x % 8; } x }

            if !self.stabilizer.input_file.read().path.is_empty() {
                let h = if target_height > 0 { target_height as u32 } else { vid.videoHeight };
                let ratio = vid.videoHeight as f64 / h as f64;
                let new_w = (vid.videoWidth as f64 / ratio).floor() as u32;
                let new_h = (vid.videoHeight as f64 / (vid.videoWidth as f64 / new_w as f64)).floor() as u32;
                ::log::info!("surface size: {}x{}", new_w, new_h);

                self.stabilizer.pose_estimator.rescale(new_w, new_h);
                self.chart_data_changed();

                vid.setSurfaceSize(new_w, new_h);
                vid.setRotation(vid.getRotation());
                vid.setCurrentFrame(vid.currentFrame);
            }
        }
    }

    fn set_integration_method(&mut self, index: usize) {
        let finished = util::qt_queued_callback(self, |this, _| {
            this.chart_data_changed();
            this.request_recompute();
        });

        let stab = self.stabilizer.clone();

        if stab.gyro.read().integration_method == index {
            return;
        }

        core::run_threaded(move || {
            {
                stab.invalidate_ongoing_computations();

                let mut gyro = stab.gyro.write();
                gyro.integration_method = index;
                gyro.integrate();
                stab.smoothing.write().update_quats_checksum(&gyro.quaternions);
            }
            stab.invalidate_smoothing();
            finished(());
        });
    }

    fn set_zero_copy(&self, player: QJSValue, enabled: bool) {
        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()

            if enabled {
                qrhi_undistort::init_player(vid.get_mdkplayer(), self.stabilizer.clone());
            } else {
                qrhi_undistort::deinit_player(vid.get_mdkplayer());
            }
            vid.setCurrentFrame(vid.currentFrame);
        }
    }

    fn set_gpu_decoding(&self, enabled: bool) {
        *rendering::GPU_DECODING.write() = enabled;
    }

    fn reset_player(&self, player: QJSValue) {
        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()
            vid.onResize(Box::new(|_, _| { }));
            vid.onProcessPixels(Box::new(|_, _, _, _, _, _| -> (u32, u32, u32, *mut u8) {
                (0, 0, 0, std::ptr::null_mut())
            }));
            qrhi_undistort::deinit_player(vid.get_mdkplayer());
        }
    }
    fn init_player(&self, player: QJSValue) {
        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()

            let bg_color = vid.getBackgroundColor().get_rgba_f();
            self.stabilizer.params.write().background = Vector4::new(bg_color.0 as f32 * 255.0, bg_color.1 as f32 * 255.0, bg_color.2 as f32 * 255.0, bg_color.3 as f32 * 255.0);

            let stab = self.stabilizer.clone();
            vid.onResize(Box::new(move |width, height| {
                let current_size = stab.params.read().size;
                if current_size.0 != width as usize || current_size.1 != height as usize {
                    stab.set_size(width as usize, height as usize);
                    stab.recompute_threaded(|_|());

                    qrhi_undistort::resize_player(stab.clone());
                }
            }));

            let stab = self.stabilizer.clone();
            let out_pixels = RefCell::new(Vec::new());
            vid.onProcessPixels(Box::new(move |_frame, timestamp_ms, width, height, stride, pixels: &mut [u8]| -> (u32, u32, u32, *mut u8) {
                // let _time = std::time::Instant::now();

                // TODO: cache in atomics instead of locking the mutex every time
                let (ow, oh) = stab.params.read().output_size;
                let os = ow * 4; // Assume RGBA8 - 4 bytes per pixel

                let mut out_pixels = out_pixels.borrow_mut();
                out_pixels.resize_with(os*oh, u8::default);


                use gyroflow_core::gpu::{ BufferDescription, BufferSource };
                let ret = stab.process_pixels((timestamp_ms * 1000.0) as i64, &mut BufferDescription {
                    input_size: (width as usize, height as usize, stride as usize),
                    output_size: (ow, oh, os),
                    buffers: BufferSource::Cpu {
                        input: pixels,
                        output: &mut out_pixels
                    },
                    input_rect: None, output_rect: None
                });

                // println!("Frame {:.3}, {}x{}, {:.2} MB | OpenCL {:.3}ms", timestamp_ms, width, height, pixels.len() as f32 / 1024.0 / 1024.0, _time.elapsed().as_micros() as f64 / 1000.0);
                if ret {
                    (ow as u32, oh as u32, os as u32, out_pixels.as_mut_ptr())
                } else {
                    (0, 0, 0, std::ptr::null_mut())
                }
            }));
        }
    }

    fn set_background_color(&mut self, color: QString, player: QJSValue) {
        if let Some(vid) = player.to_qobject::<MDKVideoItem>() {
            let vid = unsafe { &mut *vid.as_ptr() }; // vid.borrow_mut()

            let color = QColor::from_name(&color.to_string());
            vid.setBackgroundColor(color);

            let bg = color.get_rgba_f();
            self.stabilizer.set_background_color(Vector4::new(bg.0 as f32 * 255.0, bg.1 as f32 * 255.0, bg.2 as f32 * 255.0, bg.3 as f32 * 255.0));
        }
    }

    fn set_smoothing_method(&mut self, index: usize) -> QJsonArray {
        let params = util::serde_json_to_qt_array(&self.stabilizer.set_smoothing_method(index));
        self.request_recompute();
        self.chart_data_changed();
        params
    }
    fn set_smoothing_param(&mut self, name: QString, val: f64) {
        self.stabilizer.set_smoothing_param(&name.to_string(), val);
        self.chart_data_changed();
        self.request_recompute();
    }
    wrap_simple_method!(set_horizon_lock, lock_percent: f64, roll: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_use_gravity_vectors, v: bool; recompute; chart_data_changed);
    pub fn get_smoothing_algs(&self) -> QVariantList {
        self.stabilizer.get_smoothing_algs().into_iter().map(QString::from).collect()
    }
    fn get_smoothing_status(&self) -> QJsonArray {
        util::serde_json_to_qt_array(&self.stabilizer.get_smoothing_status())
    }
    fn get_smoothing_max_angles(&self) -> QJsonArray {
        let max_angles = self.stabilizer.get_smoothing_max_angles();
        util::serde_json_to_qt_array(&serde_json::json!([max_angles.0, max_angles.1, max_angles.2]))
    }

    fn recompute_threaded(&mut self) {
        let id = self.stabilizer.recompute_threaded(util::qt_queued_callback_mut(self, |this, (id, _discarded): (u64, bool)| {
            if !this.ongoing_computations.contains(&id) {
                ::log::error!("Unknown compute_id: {}", id);
            }
            this.ongoing_computations.remove(&id);
            let finished = this.ongoing_computations.is_empty();
            this.compute_progress(id, if finished { 1.0 } else { 0.0 });
        }));
        self.ongoing_computations.insert(id);

        self.compute_progress(id, 0.0);
    }

    fn cancel_current_operation(&mut self) {
        self.cancel_flag.store(true, SeqCst);
    }

    fn export_gyroflow_file(&self, thin: bool, extended: bool, additional_data: QJsonObject, override_location: QString, overwrite: bool) {
        let gf_path = if override_location.is_empty() {
            let video_path = self.stabilizer.input_file.read().path.clone();
            let video_path = std::path::Path::new(&video_path);
            video_path.with_extension("gyroflow").to_string_lossy().into()
        } else {
            override_location.to_string()
        };

        if !overwrite && std::path::Path::new(&gf_path).exists() {
            self.gyroflow_exists(QString::from(gf_path), thin, extended);
        } else {
            match self.stabilizer.export_gyroflow_file(&gf_path, thin, extended, additional_data.to_json().to_string()) {
                Ok(_) => {
                    self.message(QString::from("Gyroflow file exported to %1."), QString::from(format!("<b>{}</b>", gf_path)), QString::default());
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    self.request_location(QString::from(gf_path), thin, extended);
                },
                Err(e) => {
                    self.error(QString::from("An error occured: %1"), QString::from(e.to_string()), QString::default());
                }
            }
        }
    }

    fn export_gyroflow_data(&self, thin: bool, extended: bool, additional_data: QJsonObject) -> QString {
        QString::from(self.stabilizer.export_gyroflow_data(thin, extended, additional_data.to_json().to_string()).unwrap_or_default())
    }

    fn get_paths_from_gyroflow_file(&mut self, url: QUrl) -> QStringList {
        let mut ret = vec![QString::default(); 2];
        let path = util::url_to_path(url);
        if let Ok(data) = std::fs::read(&path) {
            let path = std::path::Path::new(&path).to_path_buf();

            if let Ok(serde_json::Value::Object(obj)) = serde_json::from_slice(&data) {
                let org_video_path = obj.get("videofile").and_then(|x| x.as_str()).unwrap_or("").to_string();

                if let Some(seq_start) = obj.get("image_sequence_start").and_then(|x| x.as_i64()) {
                    self.image_sequence_start = seq_start as i32;
                }
                if let Some(seq_fps) = obj.get("image_sequence_fps").and_then(|x| x.as_f64()) {
                    self.image_sequence_fps = seq_fps;
                }
                if !org_video_path.is_empty() {
                    let video_path = StabilizationManager::<stabilization::RGBA8>::get_new_videofile_path(&org_video_path, Some(path.clone()));
                    ret[0] = QString::from(core::util::path_to_str(&video_path));
                }

                if let Some(serde_json::Value::Object(gyro)) = obj.get("gyro_source") {
                    let gyro_path = gyro.get("filepath").and_then(|x| x.as_str()).unwrap_or("").to_string();

                    if !gyro_path.is_empty() {
                        let gyro_path = StabilizationManager::<stabilization::RGBA8>::get_new_videofile_path(&gyro_path, Some(path));
                        ret[1] = QString::from(core::util::path_to_str(&gyro_path));
                    }
                }
            }
        }
        QStringList::from_iter(ret.into_iter())
    }
    fn import_gyroflow_file(&mut self, url: QUrl) {
        let path = util::url_to_path(url);
        let progress = util::qt_queued_callback_mut(self, move |this, progress: f64| {
            this.loading_gyro_in_progress = progress < 1.0;
            this.loading_gyro_progress(progress);
            this.loading_gyro_in_progress_changed();
        });
        let finished = util::qt_queued_callback_mut(self, move |this, obj: std::io::Result<serde_json::Value>| {
            this.loading_gyro_in_progress = false;
            this.loading_gyro_progress(1.0);
            this.loading_gyro_in_progress_changed();

            let obj = this.import_gyroflow_internal(obj);
            this.gyroflow_file_loaded(obj);
        });

        let stab = self.stabilizer.clone();
        let cancel_flag = self.cancel_flag.clone();
        cancel_flag.store(true, SeqCst);
        core::run_threaded(move || {
            if Arc::strong_count(&cancel_flag) > 2 {
                // Wait for other tasks to finish
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            cancel_flag.store(false, SeqCst);
            finished(stab.import_gyroflow_file(&path, false, progress, cancel_flag));
        });
    }
    fn import_gyroflow_data(&mut self, data: QString) {
        let progress = util::qt_queued_callback_mut(self, move |this, progress: f64| {
            this.loading_gyro_in_progress = progress < 1.0;
            this.loading_gyro_progress(progress);
            this.loading_gyro_in_progress_changed();
        });
        let finished = util::qt_queued_callback_mut(self, move |this, obj: std::io::Result<serde_json::Value>| {
            this.loading_gyro_in_progress = false;
            this.loading_gyro_progress(1.0);
            this.loading_gyro_in_progress_changed();

            let obj = this.import_gyroflow_internal(obj);
            this.gyroflow_file_loaded(obj);
        });

        let stab = self.stabilizer.clone();
        let cancel_flag = self.cancel_flag.clone();
        cancel_flag.store(true, SeqCst);
        core::run_threaded(move || {
            if Arc::strong_count(&cancel_flag) > 2 {
                // Wait for other tasks to finish
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            cancel_flag.store(false, SeqCst);
            finished(stab.import_gyroflow_data(data.to_string().as_bytes(), false, None, progress, cancel_flag));
        });
    }
    fn import_gyroflow_internal(&mut self, result: std::io::Result<serde_json::Value>) -> QJsonObject {
        match result {
            Ok(thin_obj) => {
                if thin_obj.as_object().unwrap().contains_key("calibration_data") {
                    self.lens_loaded = true;
                    self.lens_changed();
                    let lens_json = self.stabilizer.lens.read().get_json().unwrap_or_default();
                    self.lens_profile_loaded(QString::from(lens_json), QString::default());
                }
                self.request_recompute();
                self.chart_data_changed();
                self.keyframes_changed();
                util::serde_json_to_qt_object(&thin_obj)
            },
            Err(e) => {
                self.error(QString::from("An error occured: %1"), QString::from(e.to_string()), QString::default());
                QJsonObject::default()
            }
        }
    }

    fn set_output_size(&self, w: usize, h: usize) {
        if self.stabilizer.set_output_size(w, h) {
            self.stabilizer.recompute_undistortion();
            self.request_recompute();
            qrhi_undistort::resize_player(self.stabilizer.clone());
        }
    }

    wrap_simple_method!(override_video_fps,         v: f64; recompute; update_offset_model);
    wrap_simple_method!(set_video_rotation,         v: f64; recompute);
    wrap_simple_method!(set_stab_enabled,           v: bool);
    wrap_simple_method!(set_show_detected_features, v: bool);
    wrap_simple_method!(set_show_optical_flow,      v: bool);
    wrap_simple_method!(set_is_superview,           v: bool);
    wrap_simple_method!(set_fov,                v: f64; recompute);
    wrap_simple_method!(set_frame_readout_time, v: f64; recompute);
    wrap_simple_method!(set_adaptive_zoom,      v: f64; recompute);
    wrap_simple_method!(set_zooming_center_x,   v: f64; recompute);
    wrap_simple_method!(set_zooming_center_y,   v: f64; recompute);
    wrap_simple_method!(set_trim_start,         v: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_trim_end,           v: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_of_method,          v: u32; recompute; chart_data_changed);

    wrap_simple_method!(set_lens_correction_amount,    v: f64; recompute);
    wrap_simple_method!(set_input_horizontal_stretch,  v: f64; recompute);
    wrap_simple_method!(set_lens_is_asymmetrical,      v: bool; recompute);
    wrap_simple_method!(set_input_vertical_stretch,    v: f64; recompute);
    wrap_simple_method!(set_background_mode,           v: i32; recompute);
    wrap_simple_method!(set_background_margin,         v: f64; recompute);
    wrap_simple_method!(set_background_margin_feather, v: f64; recompute);
    wrap_simple_method!(set_video_speed,               v: f64, s: bool, z: bool; recompute);

    wrap_simple_method!(set_offset, timestamp_us: i64, offset_ms: f64; recompute; update_offset_model);
    wrap_simple_method!(clear_offsets,; recompute; update_offset_model);
    wrap_simple_method!(remove_offset, timestamp_us: i64; recompute; update_offset_model);

    wrap_simple_method!(set_imu_lpf, v: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_imu_rotation, pitch_deg: f64, roll_deg: f64, yaw_deg: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_acc_rotation, pitch_deg: f64, roll_deg: f64, yaw_deg: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_imu_orientation, v: String; recompute; chart_data_changed);
    wrap_simple_method!(set_sync_lpf, v: f64; recompute; chart_data_changed);
    wrap_simple_method!(set_imu_bias, bx: f64, by: f64, bz: f64; recompute; chart_data_changed);
    wrap_simple_method!(recompute_gyro,; recompute; chart_data_changed);

    fn get_org_duration_ms   (&self) -> f64 { self.stabilizer.params.read().duration_ms }
    fn get_scaled_duration_ms(&self) -> f64 { self.stabilizer.params.read().get_scaled_duration_ms() }
    fn get_scaled_fps        (&self) -> f64 { self.stabilizer.params.read().get_scaled_fps() }
    fn get_current_fov       (&self) -> f64 { self.stabilizer.get_current_fov() }
    fn get_scaling_ratio     (&self) -> f64 { self.stabilizer.get_scaling_ratio() }
    fn get_min_fov           (&self) -> f64 { self.stabilizer.get_min_fov() }

    fn offset_at_video_timestamp(&self, timestamp_us: i64) -> f64 {
        self.stabilizer.offset_at_video_timestamp(timestamp_us)
    }
    fn quats_at_timestamp(&self, timestamp_us: i64) -> QVariantList {
        let gyro = self.stabilizer.gyro.read();
        let ts = timestamp_us as f64 / 1000.0 - gyro.offset_at_video_timestamp(timestamp_us as f64 / 1000.0);
        let sq = gyro.smoothed_quat_at_timestamp(ts);
        let q = gyro.org_quat_at_timestamp(ts);
        QVariantList::from_iter(vec![q.w,q.i,q.j,q.k,sq.w,sq.i,sq.j,sq.k]) // scalar first
    }
    fn set_lens_param(&self, param: QString, value: f64) {
        self.stabilizer.set_lens_param(param.to_string().as_str(), value);
        self.request_recompute();
    }

    fn check_updates(&self) {
        let update = util::qt_queued_callback_mut(self, |this, (version, changelog): (String, String)| {
            this.updates_available(QString::from(version), QString::from(changelog))
        });
        core::run_threaded(move || {
            if let Ok(Ok(body)) = ureq::get("https://api.github.com/repos/gyroflow/gyroflow/releases").call().map(|x| x.into_string()) {
                if let Ok(v) = serde_json::from_str(&body) as serde_json::Result<serde_json::Value> {
                    if let Some(obj) = v.as_array().and_then(|x| x.first()).and_then(|x| x.as_object()) {
                        let name = obj.get("name").and_then(|x| x.as_str());
                        let body = obj.get("body").and_then(|x| x.as_str());

                        if let Some(name) = name {
                            ::log::info!("Latest version: {}, current version: {}", name, util::get_version());

                            if let Ok(latest_version) = semver::Version::parse(name.trim_start_matches('v')) {
                                if let Ok(this_version) = semver::Version::parse(env!("CARGO_PKG_VERSION")) {
                                    if latest_version > this_version {
                                        update((name.to_owned(), body.unwrap_or_default().to_owned()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn init_calibrator(&self) {
        #[cfg(feature = "opencv")]
        {
            self.stabilizer.params.write().is_calibrator = true;
            *self.stabilizer.lens_calibrator.write() = Some(LensCalibrator::new());
            self.stabilizer.set_smoothing_method(2); // Plain 3D
            self.stabilizer.set_smoothing_param("time_constant", 2.0);
        }
    }

    fn start_autocalibrate(&mut self, max_points: usize, every_nth_frame: usize, iterations: usize, max_sharpness: f64, custom_timestamp_ms: f64, no_marker: bool) {
        #[cfg(feature = "opencv")]
        {
            rendering::clear_log();

            self.calib_in_progress = true;
            self.calib_in_progress_changed();
            self.calib_progress(0.0, 0.0, 0, 0, 0);

            let stab = self.stabilizer.clone();

            let (fps, frame_count, trim_start_ms, trim_end_ms, trim_ratio, input_horizontal_stretch, input_vertical_stretch) = {
                let params = stab.params.read();
                let lens = stab.lens.read();
                let input_horizontal_stretch = if lens.input_horizontal_stretch > 0.01 { lens.input_horizontal_stretch } else { 1.0 };
                let input_vertical_stretch = if lens.input_vertical_stretch > 0.01 { lens.input_vertical_stretch } else { 1.0 };
                (params.fps, params.frame_count, params.trim_start * params.duration_ms, params.trim_end * params.duration_ms, params.trim_end - params.trim_start, input_horizontal_stretch, input_vertical_stretch)
            };

            let is_forced = custom_timestamp_ms > -0.5;
            let ranges = if is_forced {
                vec![(custom_timestamp_ms - 1.0, custom_timestamp_ms + 1.0)]
            } else {
                vec![(trim_start_ms, trim_end_ms)]
            };

            let cal = stab.lens_calibrator.clone();
            if max_points > 0 {
                let mut lock = cal.write();
                let cal = lock.as_mut().unwrap();
                let saved: std::collections::BTreeMap<i32, core::calibration::Detected> = {
                    let lock = cal.image_points.read();
                    cal.forced_frames.iter().filter_map(|f| Some((*f, lock.get(f)?.clone()))).collect()
                };
                *cal.image_points.write() = saved;
                cal.max_images = max_points;
                cal.iterations = iterations;
                cal.max_sharpness = max_sharpness;
            }

            let progress = util::qt_queued_callback_mut(self, |this, (ready, total, good, rms): (usize, usize, usize, f64)| {
                this.calib_in_progress = ready < total;
                this.calib_in_progress_changed();
                this.calib_progress(ready as f64 / total as f64, rms, ready, total, good);
                if rms > 0.0 {
                    this.update_calib_model();
                }
            });
            let err = util::qt_queued_callback_mut(self, |this, (msg, mut arg): (String, String)| {
                arg.push_str("\n\n");
                arg.push_str(&rendering::get_log());

                this.error(QString::from(msg), QString::from(arg), QString::default());

                this.calib_in_progress = false;
                this.calib_in_progress_changed();
            });

            self.cancel_flag.store(false, SeqCst);
            let cancel_flag = self.cancel_flag.clone();

            let total = ((frame_count as f64 * trim_ratio) / every_nth_frame as f64) as usize;
            let total_read = Arc::new(AtomicUsize::new(0));
            let processed = Arc::new(AtomicUsize::new(0));

            let input_file = stab.input_file.read().clone();
            core::run_threaded(move || {
                let gpu_decoding = *rendering::GPU_DECODING.read();
                match VideoProcessor::from_file(&input_file.path, gpu_decoding, 0, None) {
                    Ok(mut proc) => {
                        let progress = progress.clone();
                        let err2 = err.clone();
                        let cal = cal.clone();
                        let total_read = total_read.clone();
                        let processed = processed.clone();
                        let cancel_flag2 = cancel_flag.clone();
                        proc.on_frame(move |timestamp_us, input_frame, _output_frame, converter, _rate_control| {
                            let frame = core::frame_at_timestamp(timestamp_us as f64 / 1000.0, fps);

                            if is_forced && total_read.load(SeqCst) > 0 {
                                return Ok(());
                            }

                            if (frame % every_nth_frame as i32) == 0 {
                                let mut width = (input_frame.width() as f64 * input_horizontal_stretch).round() as u32;
                                let mut height = (input_frame.height() as f64 * input_vertical_stretch).round() as u32;
                                let mut pt_scale = 1.0;
                                if height > 2160 {
                                    pt_scale = height as f32 / 2160.0;
                                    width = (width as f32 / pt_scale).round() as u32;
                                    height = (height as f32 / pt_scale).round() as u32;
                                }
                                match converter.scale(input_frame, ffmpeg_next::format::Pixel::GRAY8, width, height) {
                                    Ok(mut small_frame) => {
                                        let (width, height, stride, pixels) = (small_frame.plane_width(0), small_frame.plane_height(0), small_frame.stride(0), small_frame.data_mut(0));

                                        total_read.fetch_add(1, SeqCst);
                                        let mut lock = cal.write();
                                        let cal = lock.as_mut().unwrap();
                                        if is_forced {
                                            cal.forced_frames.insert(frame);
                                        }
                                        cal.no_marker = no_marker;
                                        cal.feed_frame(timestamp_us, frame, width, height, stride, pt_scale, pixels, cancel_flag2.clone(), total, processed.clone(), progress.clone());
                                    },
                                    Err(e) => {
                                        err2(("An error occured: %1".to_string(), e.to_string()))
                                    }
                                }
                            }
                            Ok(())
                        });
                        if let Err(e) = proc.start_decoder_only(ranges, cancel_flag.clone()) {
                            err(("An error occured: %1".to_string(), e.to_string()));
                        }
                    }
                    Err(error) => {
                        err(("An error occured: %1".to_string(), error.to_string()));
                    }
                }
                // Don't lock the UI trying to draw chessboards while we calibrate
                stab.params.write().is_calibrator = false;

                while processed.load(SeqCst) < total_read.load(SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                let mut lock = cal.write();
                let cal = lock.as_mut().unwrap();
                if let Err(e) = cal.calibrate(is_forced) {
                    err(("An error occured: %1".to_string(), format!("{:?}", e)));
                } else {
                    stab.lens.write().set_from_calibrator(cal);
                    ::log::debug!("rms: {}, used_frames: {:?}, camera_matrix: {}, coefficients: {}", cal.rms, cal.used_points.keys(), cal.k, cal.d);
                }

                progress((total, total, 0, cal.rms));

                stab.params.write().is_calibrator = true;
            });
        }
    }

    fn update_calib_model(&mut self) {
        #[cfg(feature = "opencv")]
        {
            let cal = self.stabilizer.lens_calibrator.clone();

            let used_points = cal.read().as_ref().map(|x| x.used_points.clone()).unwrap_or_default();

            self.calib_model = RefCell::new(used_points.iter().map(|(_k, v)| CalibrationItem {
                timestamp_us: v.timestamp_us,
                sharpness: v.avg_sharpness,
                is_forced: v.is_forced
            }).collect());

            util::qt_queued_callback(self, |this, _| {
                this.calib_model_updated();
            })(());
        }
    }

    fn add_calibration_point(&mut self, timestamp_us: i64, no_marker: bool) {
        dbg!(timestamp_us);

        self.start_autocalibrate(0, 1, 1, 1000.0, timestamp_us as f64 / 1000.0, no_marker);
    }
    fn remove_calibration_point(&mut self, timestamp_us: i64) {
        #[cfg(feature = "opencv")]
        {
            let cal = self.stabilizer.lens_calibrator.clone();
            let mut rms = 0.0;
            {
                let mut lock = cal.write();
                let cal = lock.as_mut().unwrap();
                let mut frame_to_remove = None;
                for x in &cal.used_points {
                    if x.1.timestamp_us == timestamp_us {
                        frame_to_remove = Some(*x.0);
                        break;
                    }
                }
                if let Some(f) = frame_to_remove {
                    cal.forced_frames.remove(&f);
                    cal.used_points.remove(&f);
                }
                if cal.calibrate(true).is_ok() {
                    rms = cal.rms;
                    self.stabilizer.lens.write().set_from_calibrator(cal);
                    ::log::debug!("rms: {}, used_frames: {:?}, camera_matrix: {}, coefficients: {}", cal.rms, cal.used_points.keys(), cal.k, cal.d);
                }
            }
            self.update_calib_model();
            if rms > 0.0 {
                self.calib_progress(1.0, rms, 1, 1, 1);
            }
        }
    }

    fn export_lens_profile_filename(&self, info: QJsonObject) -> QString {
        let info_json = info.to_json().to_string();

        if let Ok(mut profile) = core::lens_profile::LensProfile::from_json(&info_json) {
            #[cfg(feature = "opencv")]
            if let Some(ref cal) = *self.stabilizer.lens_calibrator.read() {
                profile.set_from_calibrator(cal);
            }
            return QString::from(format!("{}.json", profile.get_name()));
        }
        QString::default()
    }

    fn export_lens_profile(&mut self, url: QUrl, info: QJsonObject, upload: bool) {
        let path = util::url_to_path(url);
        let info_json = info.to_json().to_string();

        match core::lens_profile::LensProfile::from_json(&info_json) {
            Ok(mut profile) => {
                #[cfg(feature = "opencv")]
                if let Some(ref cal) = *self.stabilizer.lens_calibrator.read() {
                    profile.set_from_calibrator(cal);
                }

                match profile.save_to_file(&path) {
                    Ok(json) => {
                        ::log::debug!("Lens profile json: {}", json);
                        if upload {
                            core::run_threaded(move || {
                                if let Ok(Ok(body)) = ureq::post("https://api.gyroflow.xyz/upload_profile").set("Content-Type", "application/json; charset=utf-8").send_string(&json).map(|x| x.into_string()) {
                                    ::log::debug!("Lens profile uploaded: {}", body.as_str());
                                }
                            });
                        }
                    }
                    Err(e) => { self.error(QString::from("An error occured: %1"), QString::from(format!("{:?}", e)), QString::default()); }
                }
            },
            Err(e) => { self.error(QString::from("An error occured: %1"), QString::from(format!("{:?}", e)), QString::default()); }
        }
    }

    fn load_profiles(&self, reload_from_disk: bool) {
        let loaded = util::qt_queued_callback_mut(self, |this, all_names: QVariantList| {
            this.all_profiles_loaded(all_names)
        });
        let db = self.stabilizer.lens_profile_db.clone();
        core::run_threaded(move || {
            if reload_from_disk {
                let mut new_db = core::lens_profile_database::LensProfileDatabase::default();
                new_db.load_all();
                // Important! Disable `fetch_profiles_from_github` before running these functions
                // new_db.list_all_metadata();
                // new_db.process_adjusted_metadata();

                *db.write() = new_db;
            }

            let all_names = db.read().get_all_info().into_iter().map(|(name, file, crc, official, rating, aspect_ratio)| {
                let mut list = QVariantList::from_iter([
                    QString::from(name),
                    QString::from(file),
                    QString::from(crc)
                ].into_iter());
                list.push(official.into());
                list.push(rating.into());
                list.push(aspect_ratio.into());
                list
            }).collect();

            loaded(all_names);
        });
    }

    fn fetch_profiles_from_github(&self) {
        #[cfg(target_os = "android")]
        {
            return;
        }

        use crate::core::lens_profile_database::LensProfileDatabase;

        let update = util::qt_queued_callback_mut(self, |this, _| {
            this.lens_profiles_updated(true);
        });

        core::run_threaded(move || {
            if let Ok(Ok(body)) = ureq::get("https://api.github.com/repos/gyroflow/gyroflow/git/trees/master?recursive=1").call().map(|x| x.into_string()) {
                (|| -> Option<()> {
                    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
                    for obj in v.get("tree")?.as_array()? {
                        let obj = obj.as_object()?;
                        let path = obj.get("path")?.as_str()?;
                        if path.contains("/camera_presets/") && (path.contains(".json") || path.contains(".gyroflow")) {
                            let local_path = LensProfileDatabase::get_path().join(path.replace("resources/camera_presets/", ""));
                            if !local_path.exists() {
                                ::log::info!("Downloading lens profile {:?}", local_path.file_name()?);

                                let url = obj.get("url")?.as_str()?.to_string();
                                let _ = std::fs::create_dir_all(local_path.parent()?);
                                let update = update.clone();
                                core::run_threaded(move || {
                                    let content = ureq::get(&url)
                                        .set("Accept", "application/vnd.github.v3.raw")
                                        .call().map(|x| x.into_string());
                                    if let Ok(Ok(content)) = content {
                                        if std::fs::write(local_path, content.into_bytes()).is_ok() {
                                           update(());
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Some(())
                }());
            }
        });
    }

    fn rate_profile(&self, name: QString, json: QString, is_good: bool) {
        core::run_threaded(move || {
            let mut url = url::Url::parse(&format!("https://api.gyroflow.xyz/rate?good={}", is_good)).unwrap();
            url.query_pairs_mut().append_pair("filename", &name.to_string());

            if let Ok(Ok(body)) = ureq::request_url("POST", &url).set("Content-Type", "application/json; charset=utf-8").send_string(&json.to_string()).map(|x| x.into_string()) {
                ::log::debug!("Lens profile rated: {}", body.as_str());
            }
        });
    }
    fn request_profile_ratings(&self) {
        let update = util::qt_queued_callback_mut(self, |this, _| {
            this.lens_profiles_updated(false);
        });
        let db = self.stabilizer.lens_profile_db.clone();
        core::run_threaded(move || {
            if let Ok(Ok(body)) = ureq::get("https://api.gyroflow.xyz/rate?get_ratings=1").call().map(|x| x.into_string()) {
                db.write().set_profile_ratings(body.as_str());
                update(());
            }
        });
    }

    fn list_gpu_devices(&self) {
        let finished = util::qt_queued_callback(self, |this, list: Vec<String>| {
            this.gpu_list_loaded(util::serde_json_to_qt_array(&serde_json::json!(list)))
        });
        self.stabilizer.list_gpu_devices(finished);
    }
    fn set_device(&self, i: i32) {
        let mut l = self.stabilizer.stabilization.write();
        l.set_device(i as isize);
    }
    fn set_rendering_gpu_type_from_name(&self, name: String) {
        rendering::set_gpu_type_from_name(&name);
    }

    fn export_preset(&self, url: QUrl, content: QJsonObject) {
        let contents = content.to_json_pretty();
        if let Err(e) = std::fs::write(util::url_to_path(url), contents.to_slice()) {
            self.error(QString::from("An error occured: %1"), QString::from(e.to_string()), QString::default());
        }
    }

    fn set_keyframe(&self, typ: String, timestamp_us: i64, value: f64) {
        if let Ok(kf) = KeyframeType::from_str(&typ) {
            self.stabilizer.set_keyframe(&kf, timestamp_us, value);
            self.keyframes_changed();
            self.request_recompute();
        }
    }
    fn set_keyframe_easing(&self, typ: String, timestamp_us: i64, easing: String) {
        if let Ok(kf) = KeyframeType::from_str(&typ) {
            if let Ok(e) = Easing::from_str(&easing) {
                self.stabilizer.set_keyframe_easing(&kf, timestamp_us, e);
                self.keyframes_changed();
                self.request_recompute();
            }
        }
    }
    fn keyframe_easing(&self, typ: String, timestamp_us: i64) -> String {
        if let Ok(kf) = KeyframeType::from_str(&typ) {
            if let Some(e) = self.stabilizer.keyframe_easing(&kf, timestamp_us) {
                return e.to_string();
            }
        }
        String::new()
    }
    fn remove_keyframe(&self, typ: String, timestamp_us: i64) {
        if let Ok(kf) = KeyframeType::from_str(&typ) {
            self.stabilizer.remove_keyframe(&kf, timestamp_us);
            self.keyframes_changed();
            self.request_recompute();
        }
    }
    fn clear_keyframes_type(&self, typ: String) {
        if let Ok(kf) = KeyframeType::from_str(&typ) {
            self.stabilizer.clear_keyframes_type(&kf);
            self.keyframes_changed();
            self.request_recompute();
        }
    }
    fn keyframe_value_at_video_timestamp(&self, typ: String, timestamp_ms: f64) -> QJSValue {
        if let Ok(typ) = KeyframeType::from_str(&typ) {
            if let Some(v) = self.stabilizer.keyframe_value_at_video_timestamp(&typ, timestamp_ms) {
                return QJSValue::from(v);
            }
        }
        QJSValue::default()
    }
    fn is_keyframed(&self, typ: String) -> bool {
        if let Ok(typ) = KeyframeType::from_str(&typ) {
            return self.stabilizer.is_keyframed(&typ);
        }
        false
    }

    fn update_keyframe_values(&self, mut timestamp_ms: f64) {
        let keyframes = self.stabilizer.keyframes.read();
        timestamp_ms /= keyframes.timestamp_scale.unwrap_or(1.0);
        for kf in keyframes.get_all_keys() {
            if let Some(v) = keyframes.value_at_video_timestamp(kf, timestamp_ms) {
                self.keyframe_value_updated(kf.to_string(), v);
            }
        }
    }

    fn has_gravity_vectors(&self) -> bool {
        self.stabilizer.gyro.read().gravity_vectors.as_ref().map(|v| !v.is_empty()).unwrap_or_default()
    }

    fn check_external_sdk(&self, path: QString) -> bool {
        crate::external_sdk::requires_install(&path.to_string())
    }
    fn install_external_sdk(&self, path: QString) {
        let path_str = path.to_string();
        let progress = util::qt_queued_callback_mut(self, move |this, (percent, sdk_name, error_string): (f64, &'static str, String)| {
            this.external_sdk_progress(percent, QString::from(sdk_name), QString::from(error_string), path.clone());
        });
        crate::external_sdk::install(&path_str, progress);
    }

    fn mp4_merge(&self, file_list: QStringList) {
        let mut file_list: Vec<String> = file_list.into_iter().map(QString::to_string).collect();
        file_list.sort_by(|a, b| human_sort::compare(a, b));

        ::log::debug!("Merging files: {:?}", &file_list);
        if file_list.len() < 2 {
            self.mp4_merge_progress(1.0, QString::from("Not enough files!"), QString::default());
            return;
        }
        let p = std::path::Path::new(file_list.first().unwrap());
        let output_file = p.with_file_name(format!("{}_joined.mp4", p.file_name().unwrap().to_str().unwrap())).to_string_lossy().replace('\\', "/");
        let out = output_file.clone();
        let progress = util::qt_queued_callback_mut(self, move |this, (percent, error_string): (f64, String)| {
            this.mp4_merge_progress(percent, QString::from(error_string), QString::from(out.as_str()));
        });
        core::run_threaded(move || {
            let res = mp4_merge::join_files(&file_list, output_file, |p| progress((p.min(0.9999), String::default())));
            match res {
                Ok(_) => progress((1.0, String::default())),
                Err(e) => progress((1.0, e.to_string()))
            }
        });
    }

    // Utilities
    fn file_exists(&self, path: QString) -> bool { std::path::Path::new(&path.to_string()).exists() }
    fn file_size(&self, path: QString) -> u64 { std::fs::metadata(&path.to_string()).map(|x| x.len()).unwrap_or_default() }
    fn video_duration(&self, path: QString) -> f64 { gyroflow_core::util::get_video_metadata(&path.to_string()).map(|x| x.3).unwrap_or_default() }
    fn resolve_android_url(&mut self, url: QString) -> QString { util::resolve_android_url(url) }
    fn open_file_externally(&self, path: QString) { util::open_file_externally(path); }
    fn get_username(&self) -> QString { let realname = whoami::realname(); QString::from(if realname.is_empty() { whoami::username() } else { realname }) }
    fn url_to_path(&self, url: QUrl) -> QString { QString::from(util::url_to_path(url)) }
    fn path_to_url(&self, path: QString) -> QUrl { util::path_to_url(path) }
    fn image_to_b64(&self, img: QImage) -> QString { util::image_to_b64(img) }
    fn clear_settings(&self) { util::clear_settings() }
}
