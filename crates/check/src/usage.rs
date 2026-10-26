//! What a program needs from its host: the capabilities of the operations
//! it can reach from its entry, through every function it calls or uses as
//! a value. A function nothing reaches needs nothing, and neither does a
//! platform branch that was not selected, since it was never checked.
//!
//! Each need is placed in the program's own code: an operation inside the
//! standard library or a dependency is reported where the program's code
//! first calls into what leads to it.

use crate::hir::{self, Builtin, ExprKind, FuncId, Program};
use sayon_runtime::host::Host;
use sayon_syntax::Span;
use sayon_target::Capability;
use std::collections::VecDeque;

/// A capability the program needs, and where its own code first needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub capability: Capability,
    pub span: Span,
}

/// The capability a host primitive needs, if any but the ones every target
/// has.
pub fn host_capability(host: Host) -> Option<Capability> {
    use Capability::*;
    Some(match host {
        Host::ReadLine | Host::StandardInput => Input,
        Host::Write | Host::WriteError | Host::StandardOutput | Host::StandardError => Console,
        Host::AppendFile
        | Host::PathKind
        | Host::FileSize
        | Host::ListDirectory
        | Host::CreateDirectory
        | Host::Remove
        | Host::Rename
        | Host::CopyFile
        | Host::Open
        | Host::ReadFileBytes
        | Host::WriteFileBytes
        | Host::Walk
        | Host::IsLink => Files,
        Host::Arguments | Host::Environment | Host::CurrentDirectory => Environment,
        Host::Monotonic | Host::UnixTime => Clock,
        Host::Random => Random,
        Host::Start
        | Host::ProcessInput
        | Host::ProcessOutput
        | Host::ProcessErrors
        | Host::ProcessWait
        | Host::ProcessKill
        | Host::ProcessId => Processes,
        Host::CString
        | Host::CStringAddress
        | Host::ReadCString
        | Host::ReadBytes
        | Host::WriteBytes
        | Host::PointerOffset => Foreign,
        Host::Connect
        | Host::ConnectFinish
        | Host::Listen
        | Host::Accept
        | Host::DatagramBind
        | Host::DatagramSend
        | Host::DatagramReceive
        | Host::DatagramSender
        | Host::SocketTimeout
        | Host::LocalAddress
        | Host::PeerAddress
        | Host::FinishSending
        | Host::Resolve => Network,
        Host::Secure => Tls,
        Host::Fetch | Host::FetchHead | Host::FetchBody => Http,
        Host::WindowOpen
        | Host::WindowEvents
        | Host::WindowPresent
        | Host::WindowWidth
        | Host::WindowHeight
        | Host::WindowScale
        | Host::FontSystem => Graphics,
        Host::ZoneOffset | Host::ZoneCheck | Host::LocalZone | Host::ZonesVersion => TimeZones,
        _ => return None,
    })
}

/// The capability a built-in needs, if any but the ones every target has.
fn builtin_capability(builtin: Builtin) -> Option<Capability> {
    use Capability::*;
    Some(match builtin {
        Builtin::Print => Console,
        Builtin::ReadFile | Builtin::WriteFile => Files,
        Builtin::TaskSpawn
        | Builtin::TaskJoin
        | Builtin::TaskDetach
        | Builtin::TaskYield
        | Builtin::TaskSleep
        | Builtin::ChannelNew
        | Builtin::ChannelSend
        | Builtin::ChannelSendResult
        | Builtin::ChannelReceive
        | Builtin::ChannelClose
        | Builtin::ChannelSelect => Tasks,
        Builtin::TaskSpawnParallel => Threads,
        Builtin::RawRead | Builtin::RawWrite => Foreign,
        Builtin::Host(host) => return host_capability(host),
        _ => return None,
    })
}

/// The capabilities the program needs, each once, in the order of
/// [`Capability::ALL`]. `own` tells whether a span is in the program's own
/// code rather than the standard library or a dependency.
pub fn needs(program: &Program, own: &dyn Fn(Span) -> bool) -> Vec<Need> {
    let mut walk = Walk {
        found: Vec::new(),
        reached: vec![None; program.functions.len()],
        queue: VecDeque::new(),
    };
    for constant in &program.init_order {
        let constant = program.constant(*constant);
        walk.look(None, &mut |f| hir::visit_expr(&constant.value, f));
    }
    walk.reach(program.entry, None);
    for (_, function) in &program.finally {
        walk.reach(*function, None);
    }
    while let Some(id) = walk.queue.pop_front() {
        let function = program.function(id);
        // Code of the standard library or a dependency is placed where the
        // program's code led to it.
        let site = if own(function.span) {
            None
        } else {
            walk.reached[id.0 as usize].flatten()
        };
        walk.look(site, &mut |f| {
            for param in &function.params {
                if let Some(default) = &param.default {
                    hir::visit_expr(default, f);
                }
            }
            hir::visit_block(&function.body, f);
        });
    }
    walk.found.sort_by_key(|need| need.capability);
    walk.found
}

/// Gives each expression of a body to the function it is passed.
type Body<'a> = dyn FnMut(&mut dyn FnMut(&hir::Expr)) + 'a;

struct Walk {
    found: Vec<Need>,
    /// For each function reached, where the program's code led to it.
    reached: Vec<Option<Option<Span>>>,
    queue: VecDeque<FuncId>,
}

impl Walk {
    fn reach(&mut self, function: FuncId, site: Option<Span>) {
        let slot = &mut self.reached[function.0 as usize];
        if slot.is_none() {
            *slot = Some(site);
            self.queue.push_back(function);
        }
    }

    /// Looks through the expressions `body` gives: `site` is where the
    /// program's code leads to them, or `None` for its own code.
    fn look(&mut self, site: Option<Span>, body: &mut Body) {
        let mut calls: Vec<(FuncId, Span)> = Vec::new();
        let mut needs: Vec<(Capability, Span)> = Vec::new();
        body(&mut |expr| {
            let here = site.unwrap_or(expr.span);
            match &expr.kind {
                ExprKind::Call { function, .. }
                | ExprKind::FunctionRef { function, .. }
                | ExprKind::Closure { function, .. } => calls.push((*function, here)),
                ExprKind::Builtin { builtin, .. } => {
                    if let Some(capability) = builtin_capability(*builtin) {
                        needs.push((capability, here));
                    }
                }
                ExprKind::CallForeign { .. } => needs.push((Capability::Foreign, here)),
                _ => {}
            }
        });
        for (function, here) in calls {
            self.reach(function, Some(here));
        }
        for (capability, span) in needs {
            if !self.found.iter().any(|need| need.capability == capability) {
                self.found.push(Need { capability, span });
            }
        }
    }
}
