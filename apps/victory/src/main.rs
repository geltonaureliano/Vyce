//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
mod run;
mod shader;
mod translate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader
        /// (translated for this host's wave sizes unless --host-lanes says).
        #[arg(long)]
        pipelines: bool,
        /// Translate for a host whose waves are MIN-MAX lanes (32-32 for
        /// NVIDIA, 32-64 for AMD RDNA): shaders that need another wave size
        /// are listed as failures. Default: any wave size.
        #[arg(long, value_name = "MIN-MAX", value_parser = parse_lanes)]
        host_lanes: Option<(u32, u32)>,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        /// Game executable. Defaults to game_scarlett_master.exe in the configured game directory.
        exe: Option<PathBuf>,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Translate shader containers into the shader cache ahead of a run, and
    /// make the compute pipelines a file lists (for a new driver or build).
    Warm {
        /// Folders or files of containers (`.dxbc`).
        paths: Vec<PathBuf>,
        /// A file with the 16-digit hashes of the compute programs to make
        /// pipelines of (any text around them is skipped).
        #[arg(long)]
        pipelines: Option<PathBuf>,
        /// Worker threads (default: all logical cores but two).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Show the resolved configuration and check it.
    Config,
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            host_lanes,
        } => translate::run(paths, validate, pipelines, list, jobs, host_lanes),
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => {
            let exe = match exe {
                Some(path) => path,
                None => victory_core::Config::load()?
                    .require_game_dir()?
                    .join("game_scarlett_master.exe"),
            };
            run::run(&exe, unimplemented, strict, execute, console)
        }
        #[cfg(windows)]
        Command::Warm { paths, pipelines, jobs } => warm(&paths, pipelines.as_deref(), jobs),
        #[cfg(not(windows))]
        Command::Warm { .. } => anyhow::bail!("only on Windows"),
        Command::Config => show_config(),
    }
}

/// The caches of a run, filled without one.
#[cfg(windows)]
fn warm(paths: &[PathBuf], pipelines: Option<&std::path::Path>, jobs: Option<usize>) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let mut containers = Vec::new();
    for root in paths {
        for entry in walkdir::WalkDir::new(root).sort_by_file_name().into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "dxbc") {
                containers.push(std::fs::read(entry.path())?);
            }
        }
    }
    let text = pipelines.map(std::fs::read_to_string).transpose()?.unwrap_or_default();
    let wanted: std::collections::HashSet<&str> = text
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|w| w.len() == 16)
        .collect();
    let threads = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(2).max(1))
    });
    println!(
        "{} containers, {} compute pipelines wanted, {threads} threads",
        containers.len(),
        wanted.len()
    );
    let started = std::time::Instant::now();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let warmed = victory_hle::modules::d3d12x::warm(
        &containers,
        |hash| wanted.contains(hash),
        threads,
        |_, of| {
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % 250 == 0 {
                println!("{n} of {of}, {:.0} s", started.elapsed().as_secs_f64());
            }
        },
    );
    println!(
        "{} translated ({} from the shader cache, {} now), {} refused, {} pipelines made ({} compiled now, {} from the library), {} refused by the host, {:.0} s",
        warmed.translated,
        warmed.from_cache,
        warmed.translated_now,
        warmed.refused,
        warmed.pipelines,
        warmed.compiled,
        warmed.loaded,
        warmed.failed,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `MIN-MAX` lanes per wave, or one size `N`.
fn parse_lanes(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let n = |x: &str| {
        x.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| n.is_power_of_two() && (4..=128).contains(n))
            .ok_or_else(|| format!("{x:?} is not a wave size (4 to 128, a power of two)"))
    };
    let (a, b) = (n(a)?, n(b)?);
    if a > b {
        return Err(format!("{a} is more than {b}"));
    }
    Ok((a, b))
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}
