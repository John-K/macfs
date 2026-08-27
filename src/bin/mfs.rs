//! `mfs` — a command-line front end for the `macfs` crate.
//!
//! It exercises the whole public API — listing, reading, writing, formatting,
//! checking and installing boot blocks — so it doubles as worked documentation
//! of how the library is meant to be used. Images may be raw sector dumps or
//! DiskCopy 4.2 containers; the container is detected on open and every
//! mutating command writes the volume back in the shape it arrived in.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use macfs::{Fork, ImageFormat, MfsError, MfsVolume};

/// Every subcommand and the arguments it accepts, in the order `usage` lists
/// them. This is the single source of truth for both the overview and the
/// per-command usage line printed when a command is called wrongly.
const COMMANDS: &[(&str, &str)] = &[
    ("info", "<image>"),
    ("ls", "<image> [-l]"),
    ("cat", "<image> <name> [--rsrc]"),
    ("extract", "<image> <name> [--rsrc] [-o PATH]"),
    (
        "add",
        "<image> <hostfile> [--name N] [--type XXXX] [--creator XXXX] [--rsrc HOSTFILE]",
    ),
    ("rm", "<image> <name> [--force]"),
    ("mv", "<image> <old> <new>"),
    (
        "mkfs",
        "<image> [--size 400k|800k|BYTES] [--name NAME] [--dc42] [--force]",
    ),
    ("check", "<image>"),
    ("bootblocks", "<image> [--export FILE | --import FILE]"),
];

/// How a command can fail: with the operation refused (exit 1) or with the
/// command line itself malformed (exit 2).
enum Failure {
    Error(String),
    Usage(String),
}

impl From<MfsError> for Failure {
    fn from(e: MfsError) -> Self {
        Failure::Error(e.to_string())
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Error(e.to_string())
    }
}

type Cmd = Result<ExitCode, Failure>;

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(Failure::Error(msg)) => {
            eprintln!("mfs: {msg}");
            ExitCode::FAILURE
        }
        Err(Failure::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
    }
}

fn run(mut args: Vec<String>) -> Cmd {
    if args.is_empty() {
        return Err(Failure::Usage(usage()));
    }
    let cmd = args.remove(0);
    match cmd.as_str() {
        "help" | "-h" | "--help" => {
            println!("{}", usage());
            Ok(ExitCode::SUCCESS)
        }
        "info" => info(args),
        "ls" => ls(args),
        "cat" => cat(args),
        "extract" => extract(args),
        "add" => add(args),
        "rm" => rm(args),
        "mv" => mv(args),
        "mkfs" => mkfs(args),
        "check" => check(args),
        "bootblocks" => bootblocks(args),
        other => Err(Failure::Usage(format!(
            "mfs: unknown command {other:?}\n{}",
            usage()
        ))),
    }
}

// ------------------------------------------------------------------ commands

fn info(args: Vec<String>) -> Cmd {
    let [image] = positionals(args, "info")?;
    let vol = open(&image)?;
    let i = vol.info();
    let free_bytes = i.free_blocks as u64 * i.alloc_block_size as u64;
    println!("volume:      {}", i.name);
    println!(
        "format:      {}",
        match i.format {
            ImageFormat::Raw => "raw sector image",
            ImageFormat::DiskCopy42 => "DiskCopy 4.2",
        }
    );
    println!("created:     {}", i.created);
    println!("modified:    {}", i.modified);
    println!("files:       {}", i.file_count);
    println!("block size:  {} bytes", i.alloc_block_size);
    println!(
        "blocks:      {} total, {} free ({free_bytes} bytes)",
        i.total_blocks, i.free_blocks
    );
    println!(
        "bootable:    {}",
        if vol.boot_blocks().starts_with(b"LK") {
            "yes"
        } else {
            "no"
        }
    );
    Ok(ExitCode::SUCCESS)
}

fn ls(mut args: Vec<String>) -> Cmd {
    let long = take_flag(&mut args, "-l");
    let [image] = positionals(args, "ls")?;
    let vol = open(&image)?;
    if long {
        println!(
            "TYPE CREA  {:>8}  {:>8}  {:<19}      NAME",
            "DATA", "RSRC", "MODIFIED"
        );
    }
    for f in vol.files() {
        if long {
            println!(
                "{} {}  {:>8}  {:>8}  {}  {:<3} {}",
                code(&f.type_code),
                code(&f.creator),
                f.data_len,
                f.rsrc_len,
                f.modified,
                if f.locked { "[L]" } else { "" },
                f.name
            );
        } else {
            println!("{}", f.name);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cat(mut args: Vec<String>) -> Cmd {
    let fork = fork_flag(&mut args);
    let [image, name] = positionals(args, "cat")?;
    let bytes = open(&image)?.read_fork(&name, fork)?;
    to_stdout(&bytes)
}

fn extract(mut args: Vec<String>) -> Cmd {
    let fork = fork_flag(&mut args);
    let out = take_opt(&mut args, "-o").map_err(|e| usage_err("extract", &e))?;
    let [image, name] = positionals(args, "extract")?;
    let bytes = open(&image)?.read_fork(&name, fork)?;

    let path = out.unwrap_or_else(|| {
        let mut p = name.replace('/', "_");
        if fork == Fork::Resource {
            p.push_str(".rsrc");
        }
        p
    });
    if path == "-" {
        return to_stdout(&bytes);
    }
    write_host(&path, &bytes)?;
    println!("wrote {} bytes to {path}", bytes.len());
    Ok(ExitCode::SUCCESS)
}

fn add(mut args: Vec<String>) -> Cmd {
    let name = take_opt(&mut args, "--name").map_err(|e| usage_err("add", &e))?;
    let type_code = take_opt(&mut args, "--type").map_err(|e| usage_err("add", &e))?;
    let creator = take_opt(&mut args, "--creator").map_err(|e| usage_err("add", &e))?;
    let rsrc = take_opt(&mut args, "--rsrc").map_err(|e| usage_err("add", &e))?;
    let [image, hostfile] = positionals(args, "add")?;

    let type_code = parse_code(type_code.as_deref(), "--type")?;
    let creator = parse_code(creator.as_deref(), "--creator")?;
    let name = match name {
        Some(n) => n,
        None => basename(&hostfile),
    };
    let data = read_host(&hostfile)?;
    let rsrc = rsrc.map(|p| read_host(&p)).transpose()?;

    let mut vol = open(&image)?;
    if vol.file(&name).is_ok() {
        return Err(Failure::Error(format!(
            "file already exists: {name} (delete it first with `mfs rm`)"
        )));
    }
    vol.create_file(&name, type_code, creator)?;
    vol.write_fork(&name, Fork::Data, &data)?;
    if let Some(rsrc) = &rsrc {
        vol.write_fork(&name, Fork::Resource, rsrc)?;
    }
    vol.save_path(&image)?;
    println!(
        "added {name} ({} bytes data, {} bytes resource)",
        data.len(),
        rsrc.map_or(0, |r| r.len())
    );
    Ok(ExitCode::SUCCESS)
}

fn rm(mut args: Vec<String>) -> Cmd {
    let force = take_flag(&mut args, "--force");
    let [image, name] = positionals(args, "rm")?;
    let mut vol = open(&image)?;
    if force && vol.file(&name)?.locked {
        vol.set_locked(&name, false)?;
    }
    vol.delete_file(&name)?;
    vol.save_path(&image)?;
    println!("deleted {name}");
    Ok(ExitCode::SUCCESS)
}

fn mv(args: Vec<String>) -> Cmd {
    let [image, old, new] = positionals(args, "mv")?;
    let mut vol = open(&image)?;
    vol.rename_file(&old, &new)?;
    vol.save_path(&image)?;
    println!("renamed {old} to {new}");
    Ok(ExitCode::SUCCESS)
}

fn mkfs(mut args: Vec<String>) -> Cmd {
    let size = take_opt(&mut args, "--size").map_err(|e| usage_err("mkfs", &e))?;
    let name = take_opt(&mut args, "--name").map_err(|e| usage_err("mkfs", &e))?;
    let dc42 = take_flag(&mut args, "--dc42");
    let force = take_flag(&mut args, "--force");
    let [image] = positionals(args, "mkfs")?;

    let size = match size.as_deref() {
        None => MfsVolume::FLOPPY_400K,
        Some(s) => parse_size(s)?,
    };
    let name = name.unwrap_or_else(|| "Untitled".to_string());
    let format = if dc42 {
        ImageFormat::DiskCopy42
    } else {
        ImageFormat::Raw
    };
    if !force && Path::new(&image).exists() {
        return Err(Failure::Error(format!(
            "{image} already exists (pass --force to overwrite it)"
        )));
    }

    let mut vol = MfsVolume::format(size, &name, format)?;
    vol.save_path(&image)?;
    println!(
        "created {image}: {size} byte {} volume {name:?}",
        if dc42 { "DiskCopy 4.2" } else { "raw" }
    );
    Ok(ExitCode::SUCCESS)
}

fn check(args: Vec<String>) -> Cmd {
    let [image] = positionals(args, "check")?;
    let problems = open(&image)?.check();
    if problems.is_empty() {
        println!("ok");
        return Ok(ExitCode::SUCCESS);
    }
    for p in &problems {
        println!("{p}");
    }
    println!("{} problem(s) found", problems.len());
    Ok(ExitCode::FAILURE)
}

fn bootblocks(mut args: Vec<String>) -> Cmd {
    let export = take_opt(&mut args, "--export").map_err(|e| usage_err("bootblocks", &e))?;
    let import = take_opt(&mut args, "--import").map_err(|e| usage_err("bootblocks", &e))?;
    let [image] = positionals(args, "bootblocks")?;
    if export.is_some() && import.is_some() {
        return Err(usage_err(
            "bootblocks",
            "--export and --import are mutually exclusive",
        ));
    }
    let mut vol = open(&image)?;

    if let Some(path) = export {
        let blocks = vol.boot_blocks().to_vec();
        write_host(&path, &blocks)?;
        println!("wrote {} boot block bytes to {path}", blocks.len());
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(path) = import {
        let bytes = read_host(&path)?;
        let blocks: &[u8; 1024] = bytes.as_slice().try_into().map_err(|_| {
            Failure::Error(format!(
                "{path}: boot blocks must be exactly 1024 bytes, this file is {}",
                bytes.len()
            ))
        })?;
        vol.set_boot_blocks(blocks)?;
        vol.save_path(&image)?;
        println!("installed 1024 boot block bytes from {path}");
        return Ok(ExitCode::SUCCESS);
    }

    let blocks = vol.boot_blocks();
    if blocks.starts_with(b"LK") {
        println!("bootable: yes ('LK' boot block signature present)");
    } else if blocks.iter().all(|&b| b == 0) {
        println!("not bootable (no boot code; the boot block region is all zeros)");
    } else {
        println!("not bootable (no 'LK' signature, but the boot block region is not empty)");
    }
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------- argument parsing

/// Remove `name` from `args` if present, reporting whether it was.
fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

/// Remove `name` and its value from `args`. Both `--flag value` and
/// `--flag=value` are accepted; a trailing `--flag` with nothing after it is an
/// error.
fn take_opt(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let prefix = format!("{name}=");
    let Some(i) = args
        .iter()
        .position(|a| a == name || a.starts_with(&prefix))
    else {
        return Ok(None);
    };
    if let Some(value) = args[i].strip_prefix(&prefix) {
        let value = value.to_string();
        args.remove(i);
        return Ok(Some(value));
    }
    if i + 1 >= args.len() {
        return Err(format!("{name} needs a value"));
    }
    args.remove(i);
    Ok(Some(args.remove(i)))
}

/// Check that what is left of the command line is exactly `N` positional
/// arguments, with no unconsumed flags among them.
fn positionals<const N: usize>(args: Vec<String>, cmd: &str) -> Result<[String; N], Failure> {
    if let Some(flag) = args.iter().find(|a| a.len() > 1 && a.starts_with('-')) {
        return Err(usage_err(cmd, &format!("unknown option {flag}")));
    }
    args.try_into().map_err(|_| Failure::Usage(usage_line(cmd)))
}

/// `--rsrc` used as a bare flag, selecting which fork to operate on.
fn fork_flag(args: &mut Vec<String>) -> Fork {
    if take_flag(args, "--rsrc") {
        Fork::Resource
    } else {
        Fork::Data
    }
}

fn parse_code(value: Option<&str>, flag: &str) -> Result<[u8; 4], Failure> {
    let Some(value) = value else {
        return Ok(*b"????");
    };
    // Real type and creator codes are four MacRoman bytes; this accepts the
    // ASCII subset, which covers every code anyone types on a modern keyboard.
    if value.len() != 4 || !value.is_ascii() {
        return Err(Failure::Error(format!(
            "{flag} must be exactly four ASCII characters, got {value:?}"
        )));
    }
    let mut code = [0u8; 4];
    code.copy_from_slice(value.as_bytes());
    Ok(code)
}

/// `400k`, `800K` or a plain byte count.
fn parse_size(text: &str) -> Result<u32, Failure> {
    let (digits, scale) = match text.strip_suffix(['k', 'K']) {
        Some(d) => (d, 1024),
        None => (text, 1),
    };
    digits
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| Failure::Error(format!("--size {text:?} is not 400k, 800k or a byte count")))
}

fn usage_line(cmd: &str) -> String {
    let args = COMMANDS
        .iter()
        .find(|(name, _)| *name == cmd)
        .map_or("", |(_, args)| args);
    format!("usage: mfs {cmd} {args}")
}

fn usage_err(cmd: &str, problem: &str) -> Failure {
    Failure::Usage(format!("mfs {cmd}: {problem}\n{}", usage_line(cmd)))
}

fn usage() -> String {
    let mut text = String::from("usage: mfs <command> [arguments]\n\n");
    for (name, args) in COMMANDS {
        text.push_str(&format!("  {name:<10} {args}\n"));
    }
    text.push_str(
        "\nImages may be raw sector dumps or DiskCopy 4.2 containers; the container is\n\
         detected when the image is opened and preserved when it is written back.",
    );
    text
}

// ----------------------------------------------------------------- host I/O

fn open(image: &str) -> Result<MfsVolume, Failure> {
    MfsVolume::open_path(image).map_err(|e| Failure::Error(format!("{image}: {e}")))
}

fn read_host(path: &str) -> Result<Vec<u8>, Failure> {
    std::fs::read(path).map_err(|e| Failure::Error(format!("{path}: {e}")))
}

fn write_host(path: &str, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, bytes).map_err(|e| Failure::Error(format!("{path}: {e}")))
}

/// Fork contents go to stdout as raw bytes — they are seldom UTF-8, and a
/// resource fork never is.
fn to_stdout(bytes: &[u8]) -> Cmd {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Four-character Finder code, with anything unprintable shown as `.`.
fn code(code: &[u8; 4]) -> String {
    code.iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}
