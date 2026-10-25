// nxhost - audout.rs
// Constantin Clerc (c22dev) - 0.1
use crate::audio::{self, output::{self, Output}};
use crate::ipc::{Request, Response, Service, Stub, new_event, write_buffer};
use crate::kernel::{KERNEL, Obj};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const RATE: u32 = 48000;
const DEVICE_NAME: &[u8] = b"DeviceOut";
const PCM_INT16: u32 = 2;
const STARTED: u32 = 0;
const STOPPED: u32 = 1;
const FRAMES: usize = 480;

pub struct AudOut;

fn device_name() -> Vec<u8> {
    let mut n = vec![0u8; 0x100];
    n[..DEVICE_NAME.len()].copy_from_slice(DEVICE_NAME);
    n
}

impl Service for AudOut {
    fn name(&self) -> String {
        "audout:u".into()
    }
    fn call(&self, req: &Request, rsp: &mut Response) -> u32 {
        match req.cmd {
            0 | 2 => {
                let n = req.out_buffer().map_or(0, |b| write_buffer(&b, &device_name()) / 0x100);
                rsp.push_u32(n.min(1) as u32);
            }
            1 | 3 => {
                let channels = match req.u32(4) & 0xFFFF {
                    6 => 6,
                    _ => 2,
                };
                if let Some(b) = req.out_buffer() {
                    write_buffer(&b, &device_name());
                }
                eprintln!("[audio] audio out, {channels} channels");
                rsp.push_u32(RATE);
                rsp.push_u32(channels);
                rsp.push_u32(PCM_INT16);
                rsp.push_u32(STOPPED);
                rsp.objects.push(AudioOut::open(channels));
            }
            _ => return Stub(self.name()).call(req, rsp),
        }
        0
    }
}

struct Queued {
    tag: u64,
    samples: u64,
    len: usize,
    played: usize,
}

#[derive(Default)]
struct Queues {
    pending: VecDeque<Queued>,
    released: VecDeque<u64>,
    played_samples: u64,
}

struct AudioOut {
    channels: usize,
    event: u32,
    started: Arc<AtomicBool>,
    volume: AtomicU64,
    queues: Arc<Mutex<Queues>>,
    _output: Output,
}

impl AudioOut {
    fn open(channels: u32) -> Arc<dyn Service> {
        let channels = channels as usize;
        let event = new_event(false);
        let Some(Obj::ReadableEvent(signal)) = KERNEL.lock().get(event) else { unreachable!() };
        let started = Arc::new(AtomicBool::new(false));
        let queues = Arc::new(Mutex::new(Queues::default()));
        let (s, q) = (started.clone(), queues.clone());
        let output = audio::output::start(
            RATE,
            FRAMES,
            Box::new(move |buf: &mut [f32]| {
                buf.fill(0.0);
                if !s.load(Relaxed) {
                    return;
                }
                let mut q = q.lock().unwrap();
                let mut released = false;
                for block in buf.chunks_exact_mut(output::CHANNELS * FRAMES) {
                    for i in 0..FRAMES {
                        let Some(cur) = q.pending.front_mut() else { break };
                        let at = (cur.samples + 2 * (cur.played * channels) as u64) as *const i16;
                        for c in 0..channels {
                            block[c * FRAMES + i] = unsafe { *at.add(c) } as f32 / 32768.0;
                        }
                        cur.played += 1;
                        if cur.played == cur.len {
                            let tag = cur.tag;
                            let played = cur.len as u64;
                            q.pending.pop_front();
                            q.released.push_back(tag);
                            q.played_samples += played;
                            released = true;
                        }
                    }
                }
                drop(q);
                if released {
                    crate::kernel::signal_event(&signal);
                }
            }),
        );
        Arc::new(AudioOut { channels, event, started, volume: AtomicU64::new(1.0f32.to_bits() as u64), queues, _output: output })
    }

    fn append(&self, req: &Request) {
        let Some(b) = req.in_buffer() else { return };
        let tag = req.u64(0);
        let rd = |o: u64| unsafe { *((b.addr + o) as *const u64) };
        let (samples, size, offset) = (rd(8), rd(0x18), rd(0x20));
        let len = size.saturating_sub(offset) as usize / (2 * self.channels);
        let mut q = self.queues.lock().unwrap();
        if len == 0 {
            q.released.push_back(tag);
        } else {
            q.pending.push_back(Queued { tag, samples: samples + offset, len, played: 0 });
        }
    }

    fn released(&self, req: &Request, rsp: &mut Response) {
        let mut q = self.queues.lock().unwrap();
        let mut tags = Vec::new();
        if let Some(b) = req.out_buffer() {
            while tags.len() < b.size as usize / 8 {
                let Some(t) = q.released.pop_front() else { break };
                tags.push(t);
            }
            let mut out: Vec<u8> = tags.iter().flat_map(|t| t.to_le_bytes()).collect();
            out.resize(b.size as usize, 0);
            write_buffer(&b, &out);
        }
        rsp.push_u32(tags.len() as u32);
    }
}

impl Service for AudioOut {
    fn name(&self) -> String {
        "audio::IAudioOut".into()
    }
    fn call(&self, req: &Request, rsp: &mut Response) -> u32 {
        match req.cmd {
            0 => rsp.push_u32(if self.started.load(Relaxed) { STARTED } else { STOPPED }),
            1 => self.started.store(true, Relaxed),
            2 => self.started.store(false, Relaxed),
            3 | 7 => self.append(req),
            4 => rsp.copy_handles.push(self.event),
            5 | 8 => self.released(req, rsp),
            6 => {
                let tag = req.u64(0);
                let q = self.queues.lock().unwrap();
                rsp.push_u32(q.pending.iter().any(|b| b.tag == tag) as u32);
            }
            9 => rsp.push_u32(self.queues.lock().unwrap().pending.len() as u32),
            10 => rsp.push_u64(self.queues.lock().unwrap().played_samples),
            11 => {
                let mut q = self.queues.lock().unwrap();
                let tags: Vec<u64> = q.pending.drain(..).map(|b| b.tag).collect();
                q.released.extend(tags);
                rsp.push_u32(1);
            }
            12 => self.volume.store(req.u32(0) as u64, Relaxed),
            13 => rsp.push_u32(self.volume.load(Relaxed) as u32),
            _ => return Stub(self.name()).call(req, rsp),
        }
        0
    }
}
