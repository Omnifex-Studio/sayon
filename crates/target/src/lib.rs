//! The targets Sayon builds for: their names, the platforms and processors
//! they are for, and the capabilities each provides.
//!
//! The names are a published table: `windows` always means `windows-x64`,
//! whatever a later release adds, so a build never changes its ABI or host
//! without its command changing. A target records how far its support has
//! come; only the targets this compiler can build are offered to `build`,
//! but every target can be checked, which selects its platform branches and
//! diagnoses the capabilities it lacks.

use std::fmt;

/// What a host can do for a program. Each operation of the standard library
/// that needs the host needs one of these; the pure ones every target has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// Printing to standard output and standard errors.
    Console,
    /// Reading standard input.
    Input,
    /// Files and directories.
    Files,
    /// Environment variables, the current directory, and the program's
    /// arguments.
    Environment,
    /// Starting other programs.
    Processes,
    /// TCP and UDP sockets, and resolving host names.
    Network,
    /// The HTTP client of `standard/network`.
    Http,
    /// Secure connections: `network.connect_secure` and `connection.secure`.
    Tls,
    /// Tasks and channels: `spawn`, `sleep`, `select`, and their kin.
    Tasks,
    /// Tasks on threads of their own: `spawn_parallel`, and mutexes.
    Threads,
    /// C functions of `foreign` blocks.
    Foreign,
    /// IANA time zones of `standard/time`.
    TimeZones,
    /// Clocks and dates.
    Clock,
    /// Random numbers.
    Random,
    /// `standard/json`.
    Json,
    /// `standard/math`.
    Math,
    /// Strings, text, and encodings.
    Text,
    /// Arrays, dictionaries, and the collections of `standard/collections`.
    Collections,
}

impl Capability {
    pub const ALL: &'static [Capability] = &[
        Capability::Console,
        Capability::Input,
        Capability::Files,
        Capability::Environment,
        Capability::Processes,
        Capability::Network,
        Capability::Http,
        Capability::Tls,
        Capability::Tasks,
        Capability::Threads,
        Capability::Foreign,
        Capability::TimeZones,
        Capability::Clock,
        Capability::Random,
        Capability::Json,
        Capability::Math,
        Capability::Text,
        Capability::Collections,
    ];

    /// The name programs and manifests use.
    pub fn name(self) -> &'static str {
        match self {
            Capability::Console => "console",
            Capability::Input => "input",
            Capability::Files => "files",
            Capability::Environment => "environment",
            Capability::Processes => "processes",
            Capability::Network => "network",
            Capability::Http => "http",
            Capability::Tls => "tls",
            Capability::Tasks => "tasks",
            Capability::Threads => "threads",
            Capability::Foreign => "foreign",
            Capability::TimeZones => "time_zones",
            Capability::Clock => "clock",
            Capability::Random => "random",
            Capability::Json => "json",
            Capability::Math => "math",
            Capability::Text => "text",
            Capability::Collections => "collections",
        }
    }

    /// What the capability lets a program do, for messages such as "web
    /// programs cannot start other programs".
    pub fn description(self) -> &'static str {
        match self {
            Capability::Console => "print to a console",
            Capability::Input => "read standard input",
            Capability::Files => "use files and directories",
            Capability::Environment => {
                "read environment variables, the current directory, or their arguments"
            }
            Capability::Processes => "start other programs",
            Capability::Network => "use TCP and UDP sockets",
            Capability::Http => "make HTTP requests",
            Capability::Tls => "make secure connections (TLS)",
            Capability::Tasks => "run tasks and channels",
            Capability::Threads => "run tasks on threads of their own",
            Capability::Foreign => "call C functions",
            Capability::TimeZones => "use IANA time zones",
            Capability::Clock => "read clocks",
            Capability::Random => "make random numbers",
            Capability::Json => "use JSON",
            Capability::Math => "do mathematics",
            Capability::Text => "use strings and text",
            Capability::Collections => "use collections",
        }
    }

    pub fn named(name: &str) -> Option<Capability> {
        Capability::ALL.iter().copied().find(|c| c.name() == name)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How far support for a target has come (DESIGN's platform support
/// states). Each state includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Its ABI, capabilities, packaging, and tests are described; programs
    /// can be checked for it.
    Specified,
    /// The compiler builds programs for it.
    Buildable,
    /// Programs built for it have run the conformance tests on it.
    Executed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Specified => "specified",
            State::Buildable => "buildable",
            State::Executed => "executed",
        }
    }
}

/// A target: a platform and processor, with the ABI its programs use.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// Its name, such as `linux-x64`.
    pub name: &'static str,
    /// Its LLVM target triple.
    pub triple: &'static str,
    /// What `platform` is in its programs: `windows`, `linux`, `macos`,
    /// `android`, `ios`, `web`, or `wasi`.
    pub platform: &'static str,
    /// What `processor` is in its programs: `x64`, `arm64`, or `wasm32`.
    pub processor: &'static str,
    /// The size of an address, in bytes.
    pub pointer_size: u64,
    pub state: State,
    /// When support for it is to come further, for messages.
    pub planned: &'static str,
    pub capabilities: &'static [Capability],
}

impl Target {
    pub fn provides(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The capabilities it lacks, in the order of [`Capability::ALL`].
    pub fn lacks(&self) -> Vec<Capability> {
        Capability::ALL
            .iter()
            .copied()
            .filter(|c| !self.provides(*c))
            .collect()
    }

    /// The bits of C's `long`, which C leaves to each platform: 32 on
    /// Windows and where addresses are 32 bits, 64 elsewhere.
    pub fn c_long_bits(&self) -> u32 {
        if self.platform == "windows" || self.pointer_size == 4 {
            32
        } else {
            64
        }
    }

    /// The suffix of the programs built for it.
    pub fn executable_suffix(&self) -> &'static str {
        match self.platform {
            "windows" => ".exe",
            "web" | "wasi" => ".wasm",
            _ => "",
        }
    }

    /// Whether this is the target the running compiler was built for.
    pub fn is_host(&self) -> bool {
        std::ptr::eq(self, host())
    }

    /// Whether the compiler builds programs for it on any computer, not
    /// only on a computer of the target itself: a buildable WebAssembly
    /// target, whose programs are the same wherever they are built.
    pub fn built_anywhere(&self) -> bool {
        self.state >= State::Buildable && self.processor == "wasm32"
    }

    /// Whether the running compiler builds programs for it.
    pub fn builds_here(&self) -> bool {
        self.is_host() || self.built_anywhere()
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

use Capability::*;

/// What every desktop system provides.
const DESKTOP: &[Capability] = &[
    Console,
    Input,
    Files,
    Environment,
    Processes,
    Network,
    Http,
    Tls,
    Tasks,
    Threads,
    Foreign,
    TimeZones,
    Clock,
    Random,
    Json,
    Math,
    Text,
    Collections,
];

/// What a phone's application can use: its own files, the network, and
/// threads, but no console input, environment, or other programs. TLS and
/// time zones there come with their own adapters later.
const MOBILE: &[Capability] = &[
    Console,
    Files,
    Network,
    Http,
    Tasks,
    Threads,
    Foreign,
    Clock,
    Random,
    Json,
    Math,
    Text,
    Collections,
];

/// What a page can use: the console, the event loop for tasks, and HTTP
/// through the browser's `fetch`. Sockets, files, other programs, and C are
/// not a page's.
const WEB: &[Capability] = &[
    Console,
    Http,
    Tasks,
    Clock,
    Random,
    Json,
    Math,
    Text,
    Collections,
];

/// What WASI preview 1 offers: the console, preopened files, the
/// environment, and the C library compiled to WebAssembly; no sockets,
/// threads, or other programs. Tasks take turns there through functions
/// lowered to coroutines, since WebAssembly has no fibers.
const WASI: &[Capability] = &[
    Console,
    Input,
    Files,
    Environment,
    Tasks,
    Foreign,
    Clock,
    Random,
    Json,
    Math,
    Text,
    Collections,
];

/// Every target, in the order `sayon target list` shows them.
pub static TARGETS: &[Target] = &[
    Target {
        name: "windows-x64",
        triple: "x86_64-pc-windows-msvc",
        platform: "windows",
        processor: "x64",
        pointer_size: 8,
        state: State::Executed,
        planned: "M1",
        capabilities: DESKTOP,
    },
    Target {
        name: "linux-x64",
        triple: "x86_64-unknown-linux-gnu",
        platform: "linux",
        processor: "x64",
        pointer_size: 8,
        state: State::Executed,
        planned: "M3",
        capabilities: DESKTOP,
    },
    Target {
        name: "wasi",
        triple: "wasm32-wasip1",
        platform: "wasi",
        processor: "wasm32",
        pointer_size: 4,
        state: State::Executed,
        planned: "M4",
        capabilities: WASI,
    },
    Target {
        name: "web",
        triple: "wasm32-unknown-unknown",
        platform: "web",
        processor: "wasm32",
        pointer_size: 4,
        state: State::Executed,
        planned: "M4",
        capabilities: WEB,
    },
    Target {
        name: "android-arm64",
        triple: "aarch64-linux-android",
        platform: "android",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M4",
        capabilities: MOBILE,
    },
    Target {
        name: "android-x64",
        triple: "x86_64-linux-android",
        platform: "android",
        processor: "x64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M4",
        capabilities: MOBILE,
    },
    Target {
        name: "windows-arm64",
        triple: "aarch64-pc-windows-msvc",
        platform: "windows",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M5",
        capabilities: DESKTOP,
    },
    Target {
        name: "linux-arm64",
        triple: "aarch64-unknown-linux-gnu",
        platform: "linux",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M5",
        capabilities: DESKTOP,
    },
    Target {
        name: "macos-arm64",
        triple: "aarch64-apple-darwin",
        platform: "macos",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M7",
        capabilities: DESKTOP,
    },
    Target {
        name: "macos-x64",
        triple: "x86_64-apple-darwin",
        platform: "macos",
        processor: "x64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M7",
        capabilities: DESKTOP,
    },
    Target {
        name: "ios-arm64",
        triple: "aarch64-apple-ios",
        platform: "ios",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M7",
        capabilities: MOBILE,
    },
    Target {
        name: "ios-simulator-arm64",
        triple: "aarch64-apple-ios-simulator",
        platform: "ios",
        processor: "arm64",
        pointer_size: 8,
        state: State::Specified,
        planned: "M7",
        capabilities: MOBILE,
    },
];

/// Short names, each for one target for good.
pub const ALIASES: &[(&str, &str)] = &[
    ("windows", "windows-x64"),
    ("linux", "linux-x64"),
    ("android", "android-arm64"),
    ("macos", "macos-arm64"),
    ("ios", "ios-arm64"),
];

/// Every value `platform` can have.
pub const PLATFORMS: &[&str] = &["windows", "linux", "macos", "android", "ios", "web", "wasi"];

/// Every value `processor` can have.
pub const PROCESSORS: &[&str] = &["x64", "arm64", "wasm32"];

/// The targets `all` means when a project names none.
pub const DEFAULT_SET: &[&str] = &["windows-x64", "linux-x64"];

/// The target the running compiler was built for, which `native` names.
pub fn host() -> &'static Target {
    let triple = if cfg!(all(windows, target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    };
    TARGETS
        .iter()
        .find(|t| t.triple == triple)
        .expect("the host is a target")
}

/// The target a name means: `native`, a target's name, an alias, or a
/// triple.
pub fn find(name: &str) -> Option<&'static Target> {
    if name == "native" {
        return Some(host());
    }
    let name = ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, target)| target);
    TARGETS.iter().find(|t| t.name == name || t.triple == name)
}

/// Like [`find`], with a message that lists the names for one that is not.
pub fn resolve(name: &str) -> Result<&'static Target, String> {
    find(name).ok_or_else(|| {
        let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        format!(
            "`{name}` is not a target; the targets are native, {}, and the short names {}",
            names.join(", "),
            ALIASES
                .iter()
                .map(|(alias, _)| *alias)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_aliases_and_triples_find_targets() {
        assert_eq!(find("linux").unwrap().name, "linux-x64");
        assert_eq!(find("x86_64-pc-windows-msvc").unwrap().name, "windows-x64");
        assert_eq!(find("web").unwrap().pointer_size, 4);
        assert!(find("native").unwrap().is_host());
        assert!(find("plan9").is_none());
        assert!(resolve("plan9").unwrap_err().contains("windows-x64"));
    }

    #[test]
    fn c_types_follow_the_target() {
        assert_eq!(find("windows").unwrap().c_long_bits(), 32);
        assert_eq!(find("linux").unwrap().c_long_bits(), 64);
        assert_eq!(find("wasi").unwrap().c_long_bits(), 32);
    }

    #[test]
    fn webassembly_is_built_anywhere() {
        assert!(find("wasi").unwrap().builds_here());
        assert!(!find("android").unwrap().built_anywhere());
        assert!(find("native").unwrap().builds_here());
    }

    #[test]
    fn the_tables_agree() {
        for target in TARGETS {
            assert!(PLATFORMS.contains(&target.platform), "{}", target.name);
            assert!(PROCESSORS.contains(&target.processor), "{}", target.name);
            for pure in [Json, Math, Text, Collections, Clock, Random, Console] {
                assert!(target.provides(pure), "{} lacks {pure}", target.name);
            }
        }
        for (_, name) in ALIASES {
            assert!(find(name).is_some());
        }
        for name in DEFAULT_SET {
            assert!(find(name).unwrap().state >= State::Executed);
        }
        for capability in Capability::ALL {
            assert_eq!(Capability::named(capability.name()), Some(*capability));
        }
    }
}
