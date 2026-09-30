//! The standard library, embedded in the `ovt` binary so it works on its own.

pub struct StdFile {
    pub name: &'static str,
    /// The module name for `os.args()`-style access; `None` for files whose
    /// declarations are visible everywhere.
    pub module: Option<&'static str>,
    pub text: &'static str,
}

pub const FILES: &[StdFile] = &[
    StdFile { name: "prelude", module: None, text: include_str!("../../std/prelude.ovt") },
    StdFile { name: "str", module: None, text: include_str!("../../std/str.ovt") },
    StdFile { name: "numbers", module: None, text: include_str!("../../std/numbers.ovt") },
    StdFile { name: "array", module: None, text: include_str!("../../std/array.ovt") },
    StdFile { name: "collections", module: None, text: include_str!("../../std/collections.ovt") },
    StdFile { name: "atomic", module: None, text: include_str!("../../std/atomic.ovt") },
    StdFile { name: "chan", module: None, text: include_str!("../../std/chan.ovt") },
    StdFile { name: "shared", module: None, text: include_str!("../../std/shared.ovt") },
    StdFile { name: "os", module: Some("os"), text: include_str!("../../std/os.ovt") },
    StdFile { name: "fs", module: Some("fs"), text: include_str!("../../std/fs.ovt") },
    StdFile { name: "math", module: Some("math"), text: include_str!("../../std/math.ovt") },
    StdFile { name: "log", module: Some("log"), text: include_str!("../../std/log.ovt") },
    StdFile { name: "task", module: Some("task"), text: include_str!("../../std/task.ovt") },
    StdFile { name: "net", module: Some("net"), text: include_str!("../../std/net.ovt") },
    StdFile { name: "time", module: Some("time"), text: include_str!("../../std/time.ovt") },
    StdFile { name: "json", module: Some("json"), text: include_str!("../../std/json.ovt") },
    StdFile { name: "http", module: Some("http"), text: include_str!("../../std/http.ovt") },
];

/// Module names reserved by the standard library, including ones planned for
/// later milestones, so programs can't take them.
pub const MODULE_NAMES: &[&str] = &["math", "fs", "os", "time", "net", "http", "json", "log", "task", "ffi"];

/// The milestone that brings a module that isn't implemented yet.
pub fn planned_milestone(_module: &str) -> &'static str {
    "5"
}

/// The std file that `ovt outline <name>` shows.
pub fn outline_file(name: &str) -> Option<&'static StdFile> {
    let file = match name {
        "str" => "str",
        "array" | "[T]" => "array",
        "int" | "u8" | "f64" | "numbers" | "i32" | "u32" | "i64" | "u64" | "i16" | "u16" | "i8" | "f32" => "numbers",
        "Map" | "Set" | "collections" => "collections",
        "Atomic" | "atomic" => "atomic",
        "Chan" | "chan" => "chan",
        "Shared" | "shared" => "shared",
        "prelude" | "Err" | "ErrKind" => "prelude",
        other => other,
    };
    FILES.iter().find(|f| f.name == file)
}
