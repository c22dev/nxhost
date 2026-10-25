// nxhost - h264.rs
// Constantin Clerc (c22dev) - 0.1
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFString;
use objc2_core_video::{kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

type Ref = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const TIME_VALID: u32 = 1;
const INVALID_TIME: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    pts: CMTime,
    dts: CMTime,
}

#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, i32, u32, Ref, CMTime, CMTime);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

const ENABLE_TEMPORAL_PROCESSING: u32 = 1 << 3;
const LOCK_READ_ONLY: u64 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorNull: Ref;
    fn CFRelease(r: Ref);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: Ref,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_length_size: i32,
        out: *mut Ref,
    ) -> i32;
    fn CMVideoFormatDescriptionGetPresentationDimensions(desc: Ref, pixel_aspect: u8, clean_aperture: u8) -> CGSize;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: Ref,
        memory: *mut c_void,
        length: usize,
        block_allocator: Ref,
        custom: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut Ref,
    ) -> i32;
    fn CMSampleBufferCreateReady(
        allocator: Ref,
        data: Ref,
        format: Ref,
        samples: isize,
        timings: isize,
        timing: *const CMSampleTimingInfo,
        sizes: isize,
        size: *const usize,
        out: *mut Ref,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: Ref,
        format: Ref,
        decoder_spec: Ref,
        image_attributes: Ref,
        callback: *const OutputCallbackRecord,
        out: *mut Ref,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(session: Ref, sample: Ref, flags: u32, frame_refcon: *mut c_void, info_out: *mut u32) -> i32;
    fn VTDecompressionSessionFinishDelayedFrames(session: Ref) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: Ref) -> i32;
    fn VTDecompressionSessionInvalidate(session: Ref);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(pb: Ref, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: Ref, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: Ref, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: Ref, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: Ref, plane: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pb: Ref, plane: usize) -> usize;
}

struct Sink {
    layout: (usize, usize),
    frames: Mutex<VecDeque<(i64, Vec<u8>)>>,
}

extern "C" fn on_frame(refcon: *mut c_void, _: *mut c_void, status: i32, _: u32, image: Ref, pts: CMTime, _: CMTime) {
    let sink = unsafe { &*(refcon as *const Sink) };
    if status != 0 || image.is_null() {
        if status != 0 {
            eprintln!("[movie] h264 frame failed to decode ({status})");
        }
        return;
    }
    let (stride, rows) = sink.layout;
    let mut out = vec![0u8; stride * rows * 3 / 2];
    out[stride * rows..].fill(128);
    unsafe {
        CVPixelBufferLockBaseAddress(image, LOCK_READ_ONLY);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(image, plane);
            if base.is_null() {
                continue;
            }
            let src_stride = CVPixelBufferGetBytesPerRowOfPlane(image, plane);
            let n = (CVPixelBufferGetWidthOfPlane(image, plane) * if plane == 0 { 1 } else { 2 }).min(stride);
            let dst0 = if plane == 0 { 0 } else { stride * rows };
            for y in 0..CVPixelBufferGetHeightOfPlane(image, plane).min(if plane == 0 { rows } else { rows / 2 }) {
                std::ptr::copy_nonoverlapping(base.add(y * src_stride), out[dst0 + y * stride..].as_mut_ptr(), n);
            }
        }
        CVPixelBufferUnlockBaseAddress(image, LOCK_READ_ONLY);
    }
    let us = if pts.flags & TIME_VALID != 0 && pts.timescale > 0 { pts.value * 1_000_000 / pts.timescale as i64 } else { 0 };
    sink.frames.lock().unwrap().push_back((us, out));
}

fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let annex_b = data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]);
    let mut out = Vec::new();
    if annex_b {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        for (k, &s) in starts.iter().enumerate() {
            let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            out.push(&data[s..e]);
        }
    } else {
        let mut i = 0;
        while i + 4 <= data.len() {
            let n = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let Some(nal) = data.get(i + 4..i + 4 + n) else { break };
            out.push(nal);
            i += 4 + n;
        }
    }
    out
}

pub struct H264 {
    sps: Vec<u8>,
    pps: Vec<u8>,
    format: Ref,
    session: Ref,
    sink: Box<Sink>,
}

unsafe impl Send for H264 {}

impl H264 {
    pub fn new() -> Self {
        Self {
            sps: Vec::new(),
            pps: Vec::new(),
            format: std::ptr::null(),
            session: std::ptr::null(),
            sink: Box::new(Sink { layout: (0, 0), frames: Mutex::new(VecDeque::new()) }),
        }
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        if self.format.is_null() {
            return None;
        }
        let d = unsafe { CMVideoFormatDescriptionGetPresentationDimensions(self.format, 0, 1) };
        Some((d.width as u32, d.height as u32))
    }

    pub fn decode(&mut self, data: &[u8], us: i64, layout: fn(u32, u32) -> (u32, u32)) {
        let mut sample = Vec::new();
        for nal in nal_units(data) {
            match nal.first().map(|b| b & 0x1F) {
                Some(7) if nal != self.sps.as_slice() => {
                    self.sps = nal.to_vec();
                    self.close();
                }
                Some(8) if nal != self.pps.as_slice() => {
                    self.pps = nal.to_vec();
                    self.close();
                }
                Some(7 | 8 | 9) | None => {}
                Some(_) => {
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        if sample.is_empty() || (self.session.is_null() && !self.open(layout)) {
            return;
        }
        let pts = CMTime { value: us, timescale: 1_000_000, flags: TIME_VALID, epoch: 0 };
        let timing = CMSampleTimingInfo { duration: INVALID_TIME, pts, dts: INVALID_TIME };
        unsafe {
            let mut block: Ref = std::ptr::null();
            let mut buf: Ref = std::ptr::null();
            let p = sample.as_mut_ptr() as *mut c_void;
            if CMBlockBufferCreateWithMemoryBlock(std::ptr::null(), p, sample.len(), kCFAllocatorNull, std::ptr::null(), 0, sample.len(), 0, &mut block) != 0 {
                return;
            }
            let size = sample.len();
            if CMSampleBufferCreateReady(std::ptr::null(), block, self.format, 1, 1, &timing, 1, &size, &mut buf) == 0 {
                let status = VTDecompressionSessionDecodeFrame(self.session, buf, ENABLE_TEMPORAL_PROCESSING, std::ptr::null_mut(), std::ptr::null_mut());
                if status != 0 {
                    eprintln!("[movie] h264 decode error {status}");
                }
                CFRelease(buf);
            }
            CFRelease(block);
        }
    }

    pub fn finish(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionFinishDelayedFrames(self.session);
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            }
        }
    }

    pub fn next_frame(&mut self) -> Option<(i64, Vec<u8>)> {
        self.sink.frames.lock().unwrap().pop_front()
    }

    pub fn has_frames(&self) -> bool {
        !self.sink.frames.lock().unwrap().is_empty()
    }

    pub fn discard(&mut self) {
        self.close();
        self.sink.frames.lock().unwrap().clear();
    }

    fn open(&mut self, layout: fn(u32, u32) -> (u32, u32)) -> bool {
        if self.sps.is_empty() || self.pps.is_empty() {
            return false;
        }
        unsafe {
            let sets = [self.sps.as_ptr(), self.pps.as_ptr()];
            let sizes = [self.sps.len(), self.pps.len()];
            let mut format: Ref = std::ptr::null();
            if CMVideoFormatDescriptionCreateFromH264ParameterSets(std::ptr::null(), 2, sets.as_ptr(), sizes.as_ptr(), 4, &mut format) != 0 {
                eprintln!("[movie] h264: bad SPS/PPS");
                return false;
            }
            self.format = format;
            let (w, h) = self.dimensions().unwrap();
            let (stride, rows) = layout(w, h);
            self.sink.layout = (stride as usize, rows as usize);
            let key: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const CFString as *const NSString);
            let value = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            let attrs = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&*value as &AnyObject]);
            let record = OutputCallbackRecord { callback: on_frame, refcon: &*self.sink as *const Sink as *mut c_void };
            let mut session: Ref = std::ptr::null();
            let status = VTDecompressionSessionCreate(std::ptr::null(), format, std::ptr::null(), objc2::rc::Retained::as_ptr(&attrs) as Ref, &record, &mut session);
            if status != 0 {
                eprintln!("[movie] h264: cannot create a decoder ({status})");
                return false;
            }
            eprintln!("[movie] h264 {w}x{h}");
            self.session = session;
        }
        true
    }

    fn close(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = std::ptr::null();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
                self.format = std::ptr::null();
            }
        }
    }
}

impl Drop for H264 {
    fn drop(&mut self) {
        self.close();
    }
}
