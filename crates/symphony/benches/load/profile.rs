//! `load fold --profile <file>`: turns a CPU profile recorded by `samply record --save-only
//! --unstable-presymbolicate -o <file>.json` into folded stacks for `inferno-flamegraph`, and
//! lists the functions with the most CPU time.
//!
//! On macOS samply stops every thread once per interval and records its stack, running or not.
//! A sample whose innermost frame is one of the kernel's wait calls ([`WAITS`]) is a parked
//! thread and is dropped; every other sample counts as one interval on CPU. A stack that ends in
//! a file system call (`open`, `pwrite`, `fsync`, ...) is time inside the kernel: on CPU, or
//! waiting for the disk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Res;

/// Functions listed per table.
const TOP: usize = 40;

/// Innermost frames of a thread that is parked, not running.
const WAITS: [&str; 14] = [
    "__psynch_cvwait",
    "__psynch_mutexwait",
    "__psynch_rw_rdlock",
    "__psynch_rw_wrlock",
    "__semwait_signal",
    "__ulock_wait",
    "__ulock_wait2",
    "__workq_kernreturn",
    "__select",
    "kevent",
    "kevent64",
    "mach_msg2_trap",
    "mach_msg_trap",
    "semaphore_wait_trap",
];

/// Symbols of one binary: relative address -> the function (and the functions inlined into it at
/// that address, outermost first).
type Symbols = HashMap<u64, Vec<String>>;

fn text(value: &Value, index: &Value) -> String {
    index
        .as_u64()
        .and_then(|i| value.get(usize::try_from(i).ok()?))
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned()
}

/// Drops the hash rustc appends to a symbol (`::h0123456789abcdef`).
fn clean(symbol: &str) -> String {
    match symbol.rfind("::h") {
        Some(at)
            if symbol.len() - at == 19
                && symbol[at + 3..].bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            symbol[..at].to_owned()
        }
        _ => symbol.to_owned(),
    }
}

/// Reads the `.syms.json` sidecar: one address map per binary, keyed by its debug name.
fn load_symbols(path: &Path) -> Res<HashMap<String, Symbols>> {
    let sidecar: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let strings = &sidecar["string_table"];
    let mut libs = HashMap::new();
    for lib in sidecar["data"].as_array().into_iter().flatten() {
        let table = &lib["symbol_table"];
        let mut symbols = Symbols::new();
        for known in lib["known_addresses"].as_array().into_iter().flatten() {
            let (Some(address), Some(index)) = (known[0].as_u64(), known[1].as_u64()) else {
                continue;
            };
            let Some(entry) = table.get(usize::try_from(index)?) else {
                continue;
            };
            let outer = clean(&text(strings, &entry["symbol"]));
            // With debug info, the functions inlined at this address are listed innermost first.
            let mut frames: Vec<String> = entry["frames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|frame| frame["address"].as_u64().is_none_or(|a| a == address))
                .map(|frame| clean(&text(strings, &frame["function"])))
                .collect();
            frames.reverse();
            if frames.is_empty() {
                frames.push(outer);
            }
            symbols.insert(address, frames);
        }
        if let Some(name) = lib["debug_name"].as_str() {
            libs.insert(name.to_owned(), symbols);
        }
    }
    Ok(libs)
}

/// Folds `profile` (and its `.syms.json` sidecar) into `<profile>.folded`, writes the top
/// functions to `<profile>.top.txt`, and returns both paths.
pub fn fold(profile: &Path) -> Res<(PathBuf, PathBuf)> {
    let sidecar = profile.with_extension("syms.json");
    let symbols = load_symbols(&sidecar)?;
    let document: Value = serde_json::from_slice(&std::fs::read(profile)?)?;
    let libs: Vec<String> = document["libs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|lib| lib["debugName"].as_str().unwrap_or("?").to_owned())
        .collect();

    // Microseconds per sample.
    let interval = (document["meta"]["interval"].as_f64().unwrap_or(1.0) * 1_000.0) as u64;
    let mut folded: BTreeMap<String, u64> = BTreeMap::new();
    let mut own: HashMap<String, u64> = HashMap::new();
    let mut inclusive: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;
    // CPU time by the kernel's per-thread counters, to check the sample count against.
    let mut counted = 0u64;
    for thread in document["threads"].as_array().into_iter().flatten() {
        let strings = &thread["stringArray"];
        let frames = &thread["frameTable"];
        let funcs = &thread["funcTable"];
        let resources = &thread["resourceTable"];
        let stacks = &thread["stackTable"];
        let frame_count = frames["length"].as_u64().unwrap_or(0);
        // The frames each frame-table entry stands for, outermost first.
        let mut names: Vec<Vec<String>> = Vec::new();
        for index in 0..usize::try_from(frame_count)? {
            let func = usize::try_from(frames["func"][index].as_u64().unwrap_or(0))?;
            let fallback = text(strings, &funcs["name"][func]);
            let lib = funcs["resource"][func]
                .as_u64()
                .and_then(|resource| resources["lib"].get(usize::try_from(resource).ok()?))
                .and_then(Value::as_u64)
                .and_then(|lib| libs.get(usize::try_from(lib).ok()?));
            let resolved = frames["address"][index]
                .as_u64()
                .zip(lib)
                .and_then(|(address, lib)| symbols.get(lib)?.get(&address))
                .cloned();
            names.push(resolved.unwrap_or_else(|| vec![fallback]));
        }
        let thread_name = thread["name"].as_str().unwrap_or("thread").to_owned();
        let samples = &thread["samples"];
        let count = usize::try_from(samples["length"].as_u64().unwrap_or(0))?;
        for index in 0..count {
            counted += samples["threadCPUDelta"][index].as_u64().unwrap_or(0);
            let mut path: Vec<&str> = Vec::new();
            let mut stack = samples["stack"][index].as_u64();
            while let Some(current) = stack {
                let current = usize::try_from(current)?;
                if let Some(frame) = stacks["frame"][current].as_u64() {
                    let group = &names[usize::try_from(frame)?];
                    path.extend(group.iter().rev().map(String::as_str));
                }
                stack = stacks["prefix"][current].as_u64();
            }
            if path.first().is_none_or(|leaf| WAITS.contains(leaf)) {
                continue;
            }
            let cpu = samples["weight"][index].as_u64().unwrap_or(1) * interval;
            path.push(&thread_name);
            path.reverse();
            total += cpu;
            if let Some(leaf) = path.last() {
                *own.entry((*leaf).to_owned()).or_insert(0) += cpu;
            }
            let unique: HashSet<&str> = path.iter().copied().collect();
            for name in unique {
                *inclusive.entry(name.to_owned()).or_insert(0) += cpu;
            }
            *folded.entry(path.join(";")).or_insert(0) += cpu;
        }
    }

    let folded_path = profile.with_extension("folded");
    let mut lines = String::new();
    for (stack, cpu) in &folded {
        writeln!(lines, "{stack} {cpu}")?;
    }
    std::fs::write(&folded_path, lines)?;

    let table = |title: &str, counts: &HashMap<String, u64>| -> Res<String> {
        let mut rows: Vec<(&String, &u64)> = counts.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let mut out = format!("{title}\n");
        for (name, cpu) in rows.into_iter().take(TOP) {
            writeln!(
                out,
                "{:6.2} %  {name}",
                100.0 * *cpu as f64 / total.max(1) as f64
            )?;
        }
        Ok(out)
    };
    let top_path = profile.with_extension("top.txt");
    std::fs::write(
        &top_path,
        format!(
            "On-CPU samples: {:.2} s (the kernel's thread counters say {:.2} s)\n\n{}\n{}",
            total as f64 / 1e6,
            counted as f64 / 1e6,
            table("Self time (the function itself, callees excluded)", &own)?,
            table(
                "Inclusive time (the function and everything it calls)",
                &inclusive
            )?,
        ),
    )?;
    Ok((folded_path, top_path))
}
