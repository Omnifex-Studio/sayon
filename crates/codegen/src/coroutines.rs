//! Tasks without fibers: functions that can wait become LLVM coroutines.
//!
//! WebAssembly has one stack, so a task cannot keep its own while another
//! runs. When a program for it waits anywhere (a join, `yield`, `sleep`, or
//! a channel's send, receive, or select), each function that can reach a
//! wait, and each function used as a value, since a call through a value
//! may reach one, is lowered to a switched-resume coroutine
//! (`presplitcoroutine`), which LLVM splits into a ramp and a resume
//! function whose frame holds what lives across a suspension.
//!
//! A call keeps the usual convention, `i32 @sayon.fn.N(ptr %ret, ptr %err,
//! i64 %argc, params...)`, with one more status: 2 when the callee stopped
//! to wait, leaving the frame to resume in `@sayon_frame`. The caller then
//! suspends too, and on each resume calls `@sayon_coroutine_resume` with
//! that frame, until the callee gives its final status. A waiting operation
//! of the runtime stops the same way, leaving a null frame; resuming it
//! calls the operation again (`sayon_task_retry`). The runtime's scheduler
//! resumes a task's first function, and so its chain of callers down to the
//! operation, once the task can go on.
//!
//! A frame frees itself when its function finishes, after leaving its
//! status in `@sayon_status` and setting `@sayon_done`, so a call that
//! never stopped leaves nothing behind. Programs that never wait, and every
//! program for a target with fibers, have no coroutines at all.

use crate::Dialect;
use sayon_ir::{FunctionId, Inst, Module, Op, Rvalue, Terminator};
use std::collections::HashSet;

/// Whether an operation can wait for another task.
pub fn waits(op: &Op) -> bool {
    matches!(
        op,
        Op::Join { .. }
            | Op::Yield
            | Op::Sleep { .. }
            | Op::Send { .. }
            | Op::Receive { .. }
            | Op::Select { .. }
    )
}

/// Whether the program's tasks need coroutines: on a target without
/// fibers, when the program waits anywhere.
pub fn needed(module: &Module) -> bool {
    module.target.processor == "wasm32"
        && module
            .functions
            .iter()
            .flat_map(|f| &f.blocks)
            .any(|block| matches!(&block.term, Terminator::Op { op, .. } if waits(op)))
}

/// The functions to lower to coroutines: those that wait or call through a
/// function value, the ones used as values, and, until none is left, the
/// ones that call any of those.
pub fn functions(module: &Module) -> HashSet<FunctionId> {
    let mut lowered = HashSet::new();
    if !needed(module) {
        return lowered;
    }
    for (index, function) in module.functions.iter().enumerate() {
        for block in &function.blocks {
            for inst in &block.insts {
                if let Inst::Assign {
                    value: Rvalue::MakeClosure { function, .. },
                    ..
                } = inst
                {
                    lowered.insert(*function);
                }
            }
            if let Terminator::Op { op, .. } = &block.term
                && (waits(op) || matches!(op, Op::CallValue { .. }))
            {
                lowered.insert(FunctionId(index as u32));
            }
        }
    }
    loop {
        let before = lowered.len();
        for (index, function) in module.functions.iter().enumerate() {
            let calls_lowered = function.blocks.iter().any(|block| {
                matches!(&block.term, Terminator::Op { op: Op::Call { function, .. }, .. }
                    if lowered.contains(function))
            });
            if calls_lowered {
                lowered.insert(FunctionId(index as u32));
            }
        }
        if lowered.len() == before {
            return lowered;
        }
    }
}

/// `text` in the dialect: `llvm.coro.end` gives nothing in newer LLVM.
pub fn in_dialect(text: &str, dialect: Dialect) -> String {
    if dialect.coroutine_end_void {
        text.replace("i1 @llvm.coro.end", "void @llvm.coro.end")
    } else {
        text.to_string()
    }
}

/// What every program for a target without fibers carries, whether or not
/// it waits, since the runtime's scheduler calls `sayon_coroutine_resume`.
pub const GLUE: &str = "\
@sayon_frame = external global ptr
@sayon_done = internal global i1 false
@sayon_status = internal global i32 0

declare i32 @sayon_task_retry()
declare i32 @sayon_tasks_wait_main()
declare i32 @sayon_tasks_go_on()
declare token @llvm.coro.id(i32, ptr, ptr, ptr)
declare i1 @llvm.coro.alloc(token)
declare i64 @llvm.coro.size.i64()
declare ptr @llvm.coro.begin(token, ptr)
declare i8 @llvm.coro.suspend(token, i1)
declare ptr @llvm.coro.free(token, ptr)
declare void @llvm.coro.resume(ptr)

; Resumes a suspended frame, or with null the running task's waiting
; operation, and gives its final status, or 2 when it stopped again.
define i32 @sayon_coroutine_resume(ptr %frame) {
entry:
  %operation = icmp eq ptr %frame, null
  br i1 %operation, label %retry, label %resume
retry:
  %again = call i32 @sayon_task_retry()
  ret i32 %again
resume:
  store i1 false, ptr @sayon_done
  call void @llvm.coro.resume(ptr %frame)
  %done = load i1, ptr @sayon_done
  br i1 %done, label %finished, label %waiting
finished:
  %status = load i32, ptr @sayon_status
  ret i32 %status
waiting:
  ret i32 2
}

";

/// What a web page's glue calls to run the program: `sayon_run` starts
/// `@sayon.main`, or resumes it once the browser has waited, and gives the
/// exit status, or -1 while every task sleeps, when the glue is to wait
/// `sayon_wait_milliseconds` and call it again. A page is never blocked.
pub const WEB_ENTRY: &str = "\
@sayon.exit = internal global i32 0
@sayon.main.frame = internal global ptr null

declare void @sayon_runtime_report()

define i32 @sayon_run() {
entry:
  %frame = load ptr, ptr @sayon.main.frame
  %fresh = icmp eq ptr %frame, null
  br i1 %fresh, label %start, label %resume
start:
  %started = call i32 @sayon.main()
  br label %ran
resume:
  %resumed = call i32 @sayon_coroutine_resume(ptr %frame)
  br label %ran
ran:
  %status = phi i32 [ %started, %start ], [ %resumed, %resume ]
  %sleeping = icmp eq i32 %status, 2
  br i1 %sleeping, label %wait, label %done
wait:
  %stopped = load ptr, ptr @sayon_frame
  store ptr %stopped, ptr @sayon.main.frame
  ret i32 -1
done:
  store ptr null, ptr @sayon.main.frame
  ; Now that `main` has freed its own frame.
  call void @sayon_runtime_report()
  %code = load i32, ptr @sayon.exit
  ret i32 %code
}
";

/// The declaration of the end of a coroutine, whose form depends on the
/// LLVM version; only a program with coroutines has it.
pub const END: &str = "declare i1 @llvm.coro.end(ptr, i1, token)\n";

/// The start of a coroutine's body, after its allocas: its frame, from the
/// runtime's allocator when LLVM keeps it.
pub const PROLOGUE: &str = "\
  %coro.result = alloca i32
  %coro.id = call token @llvm.coro.id(i32 0, ptr null, ptr null, ptr null)
  %coro.need = call i1 @llvm.coro.alloc(token %coro.id)
  br i1 %coro.need, label %coro.alloc, label %coro.begin
coro.alloc:
  %coro.size = call i64 @llvm.coro.size.i64()
  %coro.memory = call ptr @sayon_alloc(i64 %coro.size)
  br label %coro.begin
coro.begin:
  %coro.place = phi ptr [ null, %entry ], [ %coro.memory, %coro.alloc ]
  %coro.frame = call ptr @llvm.coro.begin(token %coro.id, ptr %coro.place)
";

/// The end of a coroutine's body. `coro.final` leaves the status the body
/// put in `%coro.result` and frees the frame; `coro.suspended` is where
/// each suspension leaves. Both return through `coro.return`, since LLVM
/// takes only one end of a coroutine that does not unwind.
pub const EPILOGUE: &str = "\
coro.final:
  %coro.status = load i32, ptr %coro.result
  store i32 %coro.status, ptr @sayon_status
  store i1 true, ptr @sayon_done
  %coro.freed = call ptr @llvm.coro.free(token %coro.id, ptr %coro.frame)
  %coro.owned = icmp ne ptr %coro.freed, null
  br i1 %coro.owned, label %coro.release, label %coro.return
coro.release:
  %coro.bytes = call i64 @llvm.coro.size.i64()
  call void @sayon_free(ptr %coro.freed, i64 %coro.bytes)
  br label %coro.return
coro.suspended:
  store i1 false, ptr @sayon_done
  store ptr %coro.frame, ptr @sayon_frame
  br label %coro.return
coro.return:
  %coro.returned = phi i32 [ %coro.status, %coro.final ], [ %coro.status, %coro.release ], [ 2, %coro.suspended ]
  call i1 @llvm.coro.end(ptr %coro.frame, i1 false, token none)
  ret i32 %coro.returned
coro.destroyed:
  unreachable
";
