// nxhost - lib.rs
// Constantin Clerc (c22dev) - 0.1
pub mod aspect;
pub mod audio;
pub mod diag;
pub mod dnssd;
pub mod gfx;
pub mod guest;
pub mod haptics;
pub mod hle;
pub mod input;
pub mod ipc;
pub mod kernel;
pub mod loader;
pub mod mem;
pub mod movie;
pub mod remote;
pub mod services;
pub mod shm;
pub mod stubs;
pub mod nvn;
pub mod svc;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::OnceLock;

pub struct Config {
    pub patched: PathBuf,
    pub npdm: PathBuf,
    pub keys: PathBuf,
    pub base: PathBuf,
    pub update: PathBuf,
    pub save: PathBuf,
    pub sdmc: PathBuf,
    pub system: PathBuf,
    pub firmware: PathBuf,
    pub dlc: Vec<PathBuf>,
    pub modules: Vec<String>,
    pub nvn_sdk: Option<String>,
    pub movies: bool,
    pub hle: Vec<(&'static str, u64)>,
    pub precompile: bool,
    pub convert_movies: bool,
    pub fill_layouts: bool,
}

static INITS: OnceLock<Vec<u64>> = OnceLock::new();

extern "C" fn init_done() {}

extern "C" fn fini_modules() {}

extern "C" fn init_modules() {
    for &f in INITS.get().unwrap() {
        let f: extern "C" fn() = unsafe { std::mem::transmute(f as usize) };
        f();
    }
}

struct Npdm {
    priority: u8,
    core: u8,
    stack_size: u32,
}

fn read_npdm(path: &PathBuf) -> Result<Npdm> {
    let b = std::fs::read(path).with_context(|| path.display().to_string())?;
    anyhow::ensure!(b.len() >= 0x20 && &b[..4] == b"META", "not an NPDM");
    Ok(Npdm { priority: b[0xE], core: b[0xF], stack_size: u32::from_le_bytes(b[0x1C..0x20].try_into()?) })
}

fn program(n: &nxfs::Nsp) -> Result<&nxfs::nca::Nca> {
    n.find(nxfs::nca::ContentType::Program).context("no Program NCA")
}

fn load_dlc(paths: &[PathBuf], keys: &nxfs::keys::Keys) -> Vec<(u64, nxfs::storage::DynStorage)> {
    let mut out = Vec::new();
    for p in paths {
        let found = (|| -> Result<Vec<(u64, nxfs::storage::DynStorage)>> {
            let nsp = nxfs::Nsp::open(&p, keys)?;
            let mut v = Vec::new();
            for nca in nsp.ncas.iter().filter(|n| n.content_type == nxfs::nca::ContentType::PublicData) {
                let sec = nca.sections.iter().find(|s| s.fs_type == nxfs::nca::FsType::RomFs).context("no RomFS")?;
                v.push((nca.program_id, nca.fs_storage(sec)?));
            }
            Ok(v)
        })();
        match found {
            Ok(v) => {
                for (id, _) in &v {
                    eprintln!("[host] add-on content {id:016x} ({})", p.display());
                }
                out.extend(v);
            }
            Err(e) => eprintln!("[host] {}: {e:#}", p.display()),
        }
    }
    out
}

pub fn start(args: Config) -> Result<()> {
    let dir = args.patched.clone();
    let npdm = read_npdm(&args.npdm)?;

    let keys = nxfs::keys::Keys::load(&args.keys)?;
    let base = nxfs::Nsp::open(&args.base, &keys)?;
    let update = nxfs::Nsp::open(&args.update, &keys)?;
    let romfs = program(&update)?.patched_romfs(program(&base)?)?;
    eprintln!("[host] RomFS: {:.2} GB", nxfs::storage::Storage::size(&*romfs) as f64 / 1e9);
    if args.convert_movies {
        movie::webm::convert_all(romfs)?;
        std::process::exit(0);
    }
    if args.precompile {
        let modules: Vec<PathBuf> = args.modules.iter().map(|m| dir.join(format!("{m}.bin"))).collect();
        nvn::precompile(romfs, &modules)?;
        std::process::exit(0);
    }
    let romfs = if args.fill_layouts { aspect::romfs(romfs)? } else { romfs };
    let _ = services::fs::CONFIG.set(services::fs::Config {
        romfs,
        save_dir: args.save.clone(),
        sd_dir: args.sdmc.clone(),
        system_dir: args.system.clone(),
        firmware_dir: args.firmware.clone(),
        dlc: load_dlc(&args.dlc, &keys),
        keys,
    });

    if remote::enabled() {
        remote::host();
    }
    diag::install_handlers();
    guest::init_tsd();
    let _ = kernel::KERNEL.aspace.set(mem::AddressSpace::reserve()?);
    let a = kernel::aspace();
    eprintln!("[host] guest address space at {:#x}..{:#x}", a.base, a.base + a.layout.total());

    let mut hle = hle::table();
    let mut optional: Vec<&str> = hle.keys().copied().collect();
    if args.nvn_sdk.is_some() {
        hle.insert("nvnBootstrapLoader", nvn::bootstrap_loader as *const () as usize as u64);
    }
    if args.movies {
        let overrides = movie::overrides();
        optional.extend(overrides.iter().map(|o| o.0));
        hle.extend(overrides);
    }
    hle.extend(args.hle.iter().copied());
    let names: Vec<&str> = args.modules.iter().map(String::as_str).collect();
    let mods = loader::load(&dir, &names, &hle, &optional)?;
    let _ = diag::MODULES.set(
        mods.iter()
            .map(|m| {
                let mut symbols: Vec<(u64, String)> = m.dynamic.symbols.iter()
                    .filter(|s| s.defined() && !s.name.is_empty())
                    .map(|s| (s.value, nxfs::nso::demangle(&s.name)))
                    .collect();
                symbols.sort();
                diag::ModInfo { name: m.name.clone(), base: m.base, size: m.size, symbols }
            })
            .collect(),
    );
    diag::start_thread_dump();
    diag::start_watch(&mods.iter().map(|m| (m.name.clone(), m.base)).collect::<Vec<_>>());
    let _ = INITS.set(mods.iter().filter_map(|m| m.dynamic.init.map(|i| m.base + i)).collect());
    let start = loader::symbol(&mods, "_ZN2nn4init5StartEmmPFvvES2_")
        .or_else(|| loader::symbol(&mods, "_ZN2nn4init5StartEmmPFvvES2_S2_"))
        .context("nn::init::Start not found")?;
    if let Some(name) = &args.nvn_sdk {
        let sdk = mods.iter().find(|m| &m.name == name).with_context(|| format!("no {name} module"))?;
        nvn::init(sdk.base, &sdk.module_id)?;
        if std::env::var_os("NX_PREWARM").is_some() {
            nvn::prewarm();
        }
    }
    if args.movies {
        movie::init(&mods);
    }
    if let Some(v) = loader::symbol(&mods, "_ZNK2nn3mii6detail11CharInfoRaw6VerifyEv") {
        let _ = services::mii::VERIFY.set(v);
    }

    let stack_size = (npdm.stack_size as u64).next_multiple_of(mem::PAGE);
    a.set(a.stack(), stack_size, mem::Block { state: mem::state::STACK, perm: mem::perm::RW, attr: 0 })?;
    kernel::set_process_core(npdm.core as i32);
    let h = kernel::create_thread(start, 0, a.stack() + stack_size, npdm.priority as i32, npdm.core as i32)?;
    let t = kernel::thread(h).unwrap();
    eprintln!("[host] main thread {h:#x}: prio {} core {} stack {stack_size:#x}; entering nn::init::Start", npdm.priority, npdm.core);
    let main_base = mods[0].base;
    let j = kernel::start_thread(t, [h as u64, main_base, init_done as *const () as usize as u64, init_modules as *const () as usize as u64, fini_modules as *const () as usize as u64])?;
    std::thread::spawn(move || {
        j.join().ok();
        eprintln!("[host] main thread returned");
        std::process::exit(0);
    });
    Ok(())
}
