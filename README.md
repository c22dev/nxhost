# nxhost
The Switch porting toolkit foundation used to run Switch games natively on Apple Silicon Macs and iPhones

Used by [Mush8](https://github.com/c22dev/mush8) (Mario Kart 8 Deluxe) and [Velocity](https://github.com/c22dev/velocity) (Sonic Colors: Ultimate)

## How does this work ?
the Switch and Apple Silicon both run arm64, so a game's code can almost run as is. a few instructions can't run outside a Switch (x18 register, system calls, system registers), so `nxpatch` rewrites them ahead of time. then `nxhost` pretends to be the Switch's OS (Horizon) for the game, emulating its kernel, its services (files, saves, controllers, audio, network...) and NVN, its graphics API, which gets translated into Metal

No JIT is used anywhere

## Crates
- `nxfs` : dump tooling which allows to decrypt NSPs and NCAs, apply updates, extract ExeFS / RomFS / NSOs, read and rebuild SARCs and layouts
- `nxpatch` : the offline rewriter for the game's modules
- `nxabi` : the few constants `nxpatch` and `nxhost` have to agree on
- `nxshader` : Maxwell shader → Metal Shading Language translator (with `shdis`, `shmsl`... to poke at shaders by hand)
- `nxhost` : the HLE Horizon kernel and services, NVN → Metal, audio, video, input

## Porting a game
A port is basically three things :
1. extract and patch the game's code once
```
nxfs --keys prod.keys extract --base base.nsp --update upd.nsp --out work/game
nxpatch rewrite --out work/game/patched work/game/exefs/main work/game/exefs/subsdk...
```
2. a small binary that calls `nxhost::start(nxhost::Config { ... })` with the dump paths and the modules to load, then `nxhost::gfx::run_app(...)`
3. fixing everything your game does that the previous ones didn't (that's the fun part)

[Velocity](https://github.com/c22dev/velocity/blob/main/crates/velocity/src/main.rs) is the smallest example of that, around 80 lines

## Requirements
- Apple Silicon Mac, macOS 14 or later
- Rust toolchain
- Xcode 26 or later (for the Metal compiler)

## Debugging
A few env vars u can tweak :
- `NX_WINDOWED=1` : windowed instead of fullscreen
- `NX_NO_SCHED=1` : let macOS schedule the game's threads instead of our Horizon scheduler
- `NX_DUMP_MSL=1` : write every translated shader to `work/msl_rt`
- `NX_TRACE_IPC=1` : log every service call
- `NX_DUMP_AUDIO=<file>` : record the game's audio to a WAV
- `echo 1 > work/capture` while running : trace the next frame's draws and dump its render targets to `work/frame`

Contributions are welcome !
