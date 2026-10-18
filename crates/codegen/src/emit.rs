//! LLVM IR text generation.
//!
//! Each IR function becomes `i32 @sayon.fn.N(ptr %ret, ptr %err, i64 %argc,
//! params...)`, returning 0 after writing its result through `%ret` or 1
//! after storing an error through `%err`. Owned parameters are passed by
//! value; borrowed parameters as pointers. Slots are stack allocations that
//! start zeroed, so dropping a slot that never received a value (or whose
//! value moved out) does nothing.

use crate::layout::SHARED_HEADER;
use crate::layout::{Layouts, c_llvm, c_param, int_code, int_llvm, needs_drop};
use sayon_check::types::{FloatType, IntType, Type, Wrapper};
use sayon_ir::*;
use sayon_syntax::{SourceMap, Span};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Helper {
    Drop,
    Clone,
    Eq,
    Display,
    /// Destroys and frees a closure environment block.
    EnvDrop,
    /// Copies a closure environment block, returning the new one.
    EnvClone,
    /// A hash of a dictionary key.
    Hash,
}

pub struct Codegen<'m> {
    pub(crate) module: &'m Module,
    sources: &'m SourceMap,
    pub(crate) layouts: Layouts,
    constants: HashMap<Vec<u8>, String>,
    constant_defs: String,
    helpers: HashMap<(Helper, Type), String>,
    pending: VecDeque<(Helper, Type, String)>,
    pub(crate) helper_code: String,
    intrinsics: HashSet<String>,
    function_code: String,
    /// The `KeyOps` table of each dictionary key type.
    key_ops: HashMap<Type, String>,
}

/// A function body under construction.
pub(crate) struct Body {
    pub code: String,
    pub allocas: String,
    next: u32,
}

impl Body {
    pub fn new() -> Body {
        Body {
            code: String::new(),
            allocas: String::new(),
            next: 0,
        }
    }

    pub fn tmp(&mut self) -> String {
        self.next += 1;
        format!("%t{}", self.next)
    }

    pub fn label(&mut self, hint: &str) -> String {
        self.next += 1;
        format!("{hint}{}", self.next)
    }

    pub fn line(&mut self, text: impl AsRef<str>) {
        self.code.push_str("  ");
        self.code.push_str(text.as_ref());
        self.code.push('\n');
    }

    pub fn start(&mut self, label: &str) {
        let _ = writeln!(self.code, "{label}:");
    }

    pub fn alloca(&mut self, llvm_type: &str) -> String {
        self.next += 1;
        let name = format!("%a{}", self.next);
        let _ = writeln!(self.allocas, "  {name} = alloca {llvm_type}");
        name
    }

    /// Emits a counted loop; `each` receives the current index (an i64 value).
    pub fn counted_loop(&mut self, count: &str, mut each: impl FnMut(&mut Body, &str)) {
        let counter = self.alloca("i64");
        self.line(format!("store i64 0, ptr {counter}"));
        let head = self.label("loop");
        let body = self.label("body");
        let exit = self.label("done");
        self.line(format!("br label %{head}"));
        self.start(&head);
        let index = self.tmp();
        self.line(format!("{index} = load i64, ptr {counter}"));
        let more = self.tmp();
        self.line(format!("{more} = icmp slt i64 {index}, {count}"));
        self.line(format!("br i1 {more}, label %{body}, label %{exit}"));
        self.start(&body);
        each(self, &index);
        let next = self.tmp();
        self.line(format!("{next} = add i64 {index}, 1"));
        self.line(format!("store i64 {next}, ptr {counter}"));
        self.line(format!("br label %{head}"));
        self.start(&exit);
    }

    pub fn memcpy(&mut self, dst: &str, src: &str, bytes: &str) {
        self.line(format!(
            "call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {bytes}, i1 false)"
        ));
    }
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        if (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\' {
            out.push(byte as char);
        } else {
            let _ = write!(out, "\\{byte:02X}");
        }
    }
    out
}

/// An integer constant in the bit pattern of its LLVM type.
pub fn int_literal(value: i128, int: IntType) -> String {
    let bits = int.bits();
    let truncated = value & ((1i128 << bits) - 1);
    let signed = if truncated >= 1i128 << (bits - 1) {
        truncated - (1i128 << bits)
    } else {
        truncated
    };
    signed.to_string()
}

pub fn float_constant(value: f64) -> String {
    format!("0x{:016X}", value.to_bits())
}

impl<'m> Codegen<'m> {
    pub fn new(module: &'m Module, sources: &'m SourceMap) -> Codegen<'m> {
        Codegen {
            module,
            sources,
            layouts: Layouts::new(module.types.clone()),
            constants: HashMap::new(),
            constant_defs: String::new(),
            helpers: HashMap::new(),
            pending: VecDeque::new(),
            helper_code: String::new(),
            intrinsics: HashSet::new(),
            function_code: String::new(),
            key_ops: HashMap::new(),
        }
    }

    /// The table of helpers a dictionary with keys of type `key` uses:
    /// `{ size, hash, eq, clone or null, drop or null, display }`.
    pub(crate) fn key_ops(&mut self, key: &Type) -> String {
        if let Some(name) = self.key_ops.get(key) {
            return name.clone();
        }
        let name = format!("@sayon.keys.{}", self.key_ops.len());
        let hash = self.helper(Helper::Hash, key);
        let eq = self.helper(Helper::Eq, key);
        let display = self.helper(Helper::Display, key);
        let (clone, drop) = if needs_drop(key) {
            (
                self.helper(Helper::Clone, key),
                self.helper(Helper::Drop, key),
            )
        } else {
            ("null".to_string(), "null".to_string())
        };
        let size = self.layouts.size_of(key);
        let _ = writeln!(
            self.constant_defs,
            "{name} = internal constant {{ i64, ptr, ptr, ptr, ptr, ptr }} {{ i64 {size}, ptr {hash}, ptr {eq}, ptr {clone}, ptr {drop}, ptr {display} }}"
        );
        self.key_ops.insert(key.clone(), name.clone());
        name
    }

    pub(crate) fn llvm(&mut self, ty: &Type) -> String {
        self.layouts.llvm(ty)
    }

    /// A private constant byte array; returns its global name.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> String {
        if let Some(name) = self.constants.get(bytes) {
            return name.clone();
        }
        let name = format!("@.str.{}", self.constants.len());
        let _ = writeln!(
            self.constant_defs,
            "{name} = private unnamed_addr constant [{} x i8] c\"{}\"",
            bytes.len(),
            escape_bytes(bytes)
        );
        self.constants.insert(bytes.to_vec(), name.clone());
        name
    }

    pub(crate) fn c_string(&mut self, text: &str) -> String {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.bytes(&bytes)
    }

    /// A static string value: the body of a `%sayon.str` constant.
    pub(crate) fn str_constant(&mut self, text: &str) -> String {
        if text.is_empty() {
            return "{ ptr null, i64 0, i64 0 }".to_string();
        }
        let name = self.bytes(text.as_bytes());
        format!("{{ ptr {name}, i64 {}, i64 0 }}", text.len())
    }

    pub(crate) fn location(&mut self, span: Span) -> String {
        let file = self.sources.get(span.file);
        let (line, column) = file.line_col(span.start);
        let text = format!("{}:{line}:{column}", file.name);
        self.c_string(&text)
    }

    pub(crate) fn intrinsic(&mut self, declaration: String) {
        self.intrinsics.insert(declaration);
    }

    /// The name of a per-type helper function, generating it on first use.
    pub(crate) fn helper(&mut self, kind: Helper, ty: &Type) -> String {
        if let Some(name) = self.helpers.get(&(kind, ty.clone())) {
            return name.clone();
        }
        let prefix = match kind {
            Helper::Drop => "drop",
            Helper::Clone => "clone",
            Helper::Eq => "eq",
            Helper::Display => "display",
            Helper::EnvDrop => "envdrop",
            Helper::EnvClone => "envclone",
            Helper::Hash => "hash",
        };
        let name = format!("@sayon.{prefix}.{}", self.helpers.len());
        self.helpers.insert((kind, ty.clone()), name.clone());
        self.pending.push_back((kind, ty.clone(), name.clone()));
        name
    }

    pub fn emit(mut self) -> String {
        let module = self.module;
        for (index, function) in module.functions.iter().enumerate() {
            let code = FnEmit::new(&mut self, function).emit(index);
            self.function_code.push_str(&code);
        }
        let main = self.emit_main();
        while let Some((kind, ty, name)) = self.pending.pop_front() {
            self.emit_helper(kind, &ty, &name);
        }

        let mut names = Vec::new();
        for member in &module.union_members {
            let constant = self.str_constant(&member.type_name());
            names.push(format!("%sayon.str {constant}"));
        }

        let mut out = String::new();
        out.push_str("; Generated by the Sayon compiler.\n\n");
        out.push_str("%sayon.str = type { ptr, i64, i64 }\n");
        out.push_str("%sayon.array = type { ptr, i64, i64 }\n");
        out.push_str("%sayon.range = type { i64, i64, i64 }\n");
        out.push_str("%sayon.func = type { ptr, ptr }\n");
        out.push_str("%sayon.view = type { ptr, i64 }\n");
        for definition in &self.layouts.definitions {
            out.push_str(definition);
            out.push('\n');
        }
        out.push('\n');
        for (index, global) in module.globals.iter().enumerate() {
            let llvm = self.layouts.llvm(&global.ty);
            let _ = writeln!(out, "@g{index} = internal global {llvm} zeroinitializer");
        }
        let _ = writeln!(
            out,
            "@sayon.type_names = internal constant [{} x %sayon.str] [{}]",
            names.len(),
            names.join(", ")
        );
        out.push_str(&self.constant_defs);
        out.push('\n');
        out.push_str(RUNTIME_DECLARATIONS);
        // Each C function once, whichever declarations name it.
        let mut declared = HashSet::new();
        for function in &module.foreign {
            if !declared.insert(function.name.as_str()) {
                continue;
            }
            let mut params: Vec<String> = function.params.iter().map(c_param).collect();
            if function.variadic {
                params.push("...".to_string());
            }
            let _ = writeln!(
                out,
                "declare {} @{}({})",
                c_llvm(&function.ret),
                function.name,
                params.join(", ")
            );
        }
        if module.foreign.iter().any(|f| f.library == "c") && !declared.contains("fflush") {
            out.push_str("declare i32 @fflush(ptr)\n");
        }
        let mut intrinsics: Vec<&String> = self.intrinsics.iter().collect();
        intrinsics.sort();
        for declaration in intrinsics {
            out.push_str(declaration);
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&self.function_code);
        out.push_str(&self.helper_code);
        out.push_str(&main);
        out
    }

    /// Destroys every module constant, in reverse initialization order.
    fn release_globals(&mut self) -> String {
        let module = self.module;
        let mut code = String::new();
        for (global, _) in module.initializers.iter().rev() {
            let ty = &module.globals[global.0 as usize].ty;
            if needs_drop(ty) {
                let drop = self.helper(Helper::Drop, ty);
                let _ = writeln!(code, "  call void {drop}(ptr @g{})", global.0);
            }
        }
        code
    }

    fn emit_main(&mut self) -> String {
        let module = self.module;
        let mut body = Body::new();
        let error = body.alloca("ptr");
        body.line(format!("store ptr null, ptr {error}"));
        body.line("call void @sayon_runtime_init()");
        let failed = "uncaught";
        for (global, initializer) in &module.initializers {
            let status = body.tmp();
            body.line(format!(
                "{status} = call i32 @sayon.fn.{}(ptr @g{}, ptr {error}, i64 0)",
                initializer.0, global.0
            ));
            let ok = body.tmp();
            body.line(format!("{ok} = icmp eq i32 {status}, 0"));
            let next = body.label("init");
            body.line(format!("br i1 {ok}, label %{next}, label %{failed}"));
            body.start(&next);
        }
        let entry = module.function(module.entry);
        let ret_llvm = self.llvm(&entry.ret);
        let result = body.alloca(&ret_llvm);
        body.line(format!("store {ret_llvm} zeroinitializer, ptr {result}"));
        let status = body.tmp();
        body.line(format!(
            "{status} = call i32 @sayon.fn.{}(ptr {result}, ptr {error}, i64 0)",
            module.entry.0
        ));
        let ok = body.tmp();
        body.line(format!("{ok} = icmp eq i32 {status}, 0"));
        body.line(format!("br i1 {ok}, label %finished, label %{failed}"));
        body.start("finished");
        let release_globals = self.release_globals();
        // Tasks still running are cancelled and cleaned up first; a task's
        // `os.exit`, or one that failed unjoined, decides the status.
        let tasks = body.tmp();
        body.line(format!("{tasks} = call i32 @sayon_tasks_finish(i1 false)"));
        let decided = body.tmp();
        body.line(format!("{decided} = icmp sge i32 {tasks}, 0"));
        body.line(format!(
            "br i1 {decided}, label %tasks_decided, label %main_decides"
        ));
        body.start("tasks_decided");
        body.code.push_str(&release_globals);
        body.line("call void @sayon_runtime_finish()");
        body.line(format!("ret i32 {tasks}"));
        body.start("main_decides");
        body.code.push_str(&release_globals);
        body.line("call void @sayon_runtime_finish()");
        if let Type::Int(int) = &entry.ret {
            let value = body.tmp();
            body.line(format!("{value} = load {}, ptr {result}", int_llvm(*int)));
            let wide = if int.bits() == 64 {
                value.clone()
            } else {
                let extended = body.tmp();
                let instruction = if int.signed() { "sext" } else { "zext" };
                body.line(format!(
                    "{extended} = {instruction} {} {value} to i64",
                    int_llvm(*int)
                ));
                extended
            };
            let code = body.tmp();
            body.line(format!("{code} = call i32 @sayon_exit_status(i64 {wide})"));
            body.line(format!("ret i32 {code}"));
        } else {
            body.line("ret i32 0");
        }
        body.start(failed);
        // The tasks stop first, so their cleanup comes before the report;
        // a task's `os.exit` replaces it.
        let tasks = body.tmp();
        body.line(format!("{tasks} = call i32 @sayon_tasks_finish(i1 true)"));
        let exiting = body.tmp();
        body.line(format!("{exiting} = icmp sge i32 {tasks}, 0"));
        body.line(format!("br i1 {exiting}, label %task_exit, label %report"));
        body.start("task_exit");
        body.line(format!("call void @sayon_error_drop(ptr {error})"));
        body.code.push_str(&release_globals);
        body.line("call void @sayon_runtime_finish()");
        body.line(format!("ret i32 {tasks}"));
        body.start("report");
        let problem = body.tmp();
        body.line(format!("{problem} = load ptr, ptr {error}"));
        let code = body.tmp();
        body.line(format!(
            "{code} = call i32 @sayon_report_uncaught(ptr {problem})"
        ));
        body.code.push_str(&release_globals);
        body.line("call void @sayon_runtime_finish()");
        body.line(format!("ret i32 {code}"));
        format!(
            "define i32 @main(i32 %argc, ptr %argv) {{\nentry:\n{}{}}}\n",
            body.allocas, body.code
        )
    }
}

pub(crate) struct FnEmit<'c, 'm> {
    pub cg: &'c mut Codegen<'m>,
    f: &'m Function,
    pub body: Body,
    /// Owned temporaries to destroy once the current instruction completes.
    after: Vec<(String, Type)>,
}

impl<'c, 'm> FnEmit<'c, 'm> {
    fn new(cg: &'c mut Codegen<'m>, f: &'m Function) -> FnEmit<'c, 'm> {
        FnEmit {
            cg,
            f,
            body: Body::new(),
            after: Vec::new(),
        }
    }

    fn slot(&self, slot: SlotId) -> String {
        format!("%s{}", slot.0)
    }

    fn error_slot(&self) -> String {
        self.slot(self.f.error_slot)
    }

    fn emit(mut self, index: usize) -> String {
        let f = self.f;
        let mut params = vec![
            "ptr %ret".to_string(),
            "ptr %err".to_string(),
            "i64 %argc".to_string(),
        ];
        for (position, slot) in f.params.iter().enumerate() {
            let definition = f.slot(*slot);
            let llvm = match definition.kind {
                SlotKind::Ref { .. } => "ptr".to_string(),
                SlotKind::Value => self.cg.llvm(&definition.ty),
            };
            params.push(format!("{llvm} %p{position}"));
        }
        for (id, definition) in f.slots.iter().enumerate() {
            let llvm = match definition.kind {
                SlotKind::Ref { .. } => "ptr".to_string(),
                SlotKind::Value => self.cg.llvm(&definition.ty),
            };
            let _ = writeln!(self.body.allocas, "  %s{id} = alloca {llvm}");
            let _ = writeln!(
                self.body.allocas,
                "  store {llvm} zeroinitializer, ptr %s{id}"
            );
        }
        for (position, slot) in f.params.iter().enumerate() {
            let definition = f.slot(*slot);
            let llvm = match definition.kind {
                SlotKind::Ref { .. } => "ptr".to_string(),
                SlotKind::Value => self.cg.llvm(&definition.ty),
            };
            let _ = writeln!(
                self.body.allocas,
                "  store {llvm} %p{position}, ptr %s{}",
                slot.0
            );
        }
        let _ = writeln!(
            self.body.allocas,
            "  store i64 %argc, ptr %s{}",
            f.argc_slot.0
        );

        for (block_index, block) in f.blocks.iter().enumerate() {
            self.body.start(&format!("b{block_index}"));
            for inst in &block.insts {
                self.inst(inst);
            }
            self.terminator(&block.term);
        }
        format!(
            "; {}\ndefine internal i32 @sayon.fn.{index}({}) {{\nentry:\n{}  br label %b0\n{}}}\n\n",
            f.name.replace('\n', " "),
            params.join(", "),
            self.body.allocas,
            self.body.code
        )
    }

    // ----- places and operands ----------------------------------------------------

    pub fn place_type(&self, place: &Place) -> Type {
        let mut ty = match &place.base {
            Base::Slot(slot) => self.f.slot(*slot).ty.clone(),
            Base::Global(global) => self.cg.module.globals[global.0 as usize].ty.clone(),
        };
        for projection in &place.projection {
            ty = self.cg.module.project_type(&ty, projection);
        }
        ty
    }

    /// A pointer to the place, following reference slots.
    pub fn place_ptr(&mut self, place: &Place) -> (String, Type) {
        let (mut pointer, mut ty) = match &place.base {
            Base::Slot(slot) => {
                let definition = self.f.slot(*slot);
                match definition.kind {
                    SlotKind::Ref { .. } => {
                        let loaded = self.body.tmp();
                        self.body
                            .line(format!("{loaded} = load ptr, ptr %s{}", slot.0));
                        (loaded, definition.ty.clone())
                    }
                    SlotKind::Value => (self.slot(*slot), definition.ty.clone()),
                }
            }
            Base::Global(global) => (
                format!("@g{}", global.0),
                self.cg.module.globals[global.0 as usize].ty.clone(),
            ),
        };
        for projection in &place.projection {
            match projection {
                Projection::Index(slot) => {
                    let (Type::Array(element) | Type::View(element, _)) = ty else {
                        panic!("index into {ty}")
                    };
                    let data = self.body.tmp();
                    self.body.line(format!("{data} = load ptr, ptr {pointer}"));
                    let index = self.body.tmp();
                    self.body
                        .line(format!("{index} = load i64, ptr %s{}", slot.0));
                    let element_llvm = self.cg.llvm(&element);
                    let address = self.body.tmp();
                    self.body.line(format!(
                        "{address} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {index}"
                    ));
                    pointer = address;
                    ty = *element;
                }
                Projection::DictValue(slot) | Projection::DictKey(slot) => {
                    let Type::Dict(key, value) = ty else {
                        panic!("dictionary access into {ty}")
                    };
                    let handle = self.body.tmp();
                    self.body
                        .line(format!("{handle} = load ptr, ptr {pointer}"));
                    let index = self.body.tmp();
                    self.body
                        .line(format!("{index} = load i64, ptr %s{}", slot.0));
                    let address = self.body.tmp();
                    let (function, result) = match projection {
                        Projection::DictKey(_) => ("sayon_dict_key_ptr", *key),
                        _ => ("sayon_dict_value_ptr", *value),
                    };
                    self.body.line(format!(
                        "{address} = call ptr @{function}(ptr {handle}, i64 {index})"
                    ));
                    pointer = address;
                    ty = result;
                }
                Projection::Payload(tag) => {
                    let address = self.body.tmp();
                    self.body.line(format!(
                        "{address} = getelementptr inbounds i8, ptr {pointer}, i64 8"
                    ));
                    pointer = address;
                    ty = self
                        .cg
                        .module
                        .union_member(*tag)
                        .cloned()
                        .unwrap_or(Type::Unknown);
                }
                Projection::Field(field) => {
                    let Type::Named(named) = &ty else {
                        panic!("field of {ty}")
                    };
                    let offset = self.cg.layouts.field_offset(named, *field);
                    let next = self.cg.module.project_type(&ty, projection);
                    pointer = self.offset_pointer(&pointer, offset);
                    ty = next;
                }
                Projection::Deref => {
                    let Type::Wrapper(Wrapper::Shared, inner) = ty else {
                        panic!("shared value of {ty}")
                    };
                    let block = self.body.tmp();
                    self.body.line(format!("{block} = load ptr, ptr {pointer}"));
                    pointer = self.offset_pointer(&block, SHARED_HEADER);
                    ty = *inner;
                }
                Projection::VariantField { variant, field } => {
                    let Type::Named(named) = &ty else {
                        panic!("variant field of {ty}")
                    };
                    let offset = self
                        .cg
                        .layouts
                        .variant_field_offset(named, *variant, *field);
                    let next = self.cg.module.project_type(&ty, projection);
                    pointer = self.offset_pointer(&pointer, offset);
                    ty = next;
                }
            }
        }
        (pointer, ty)
    }

    /// `pointer + offset` bytes.
    fn offset_pointer(&mut self, pointer: &str, offset: u64) -> String {
        if offset == 0 {
            return pointer.to_string();
        }
        let address = self.body.tmp();
        self.body.line(format!(
            "{address} = getelementptr inbounds i8, ptr {pointer}, i64 {offset}"
        ));
        address
    }

    fn const_value(&mut self, constant: &Constant) -> (String, String, Type) {
        match constant {
            Constant::Int(value, int) => (
                int_llvm(*int).to_string(),
                int_literal(*value, *int),
                Type::Int(*int),
            ),
            Constant::Float(value, FloatType::F64) => {
                ("double".into(), float_constant(*value), Type::FLOAT)
            }
            Constant::Float(value, FloatType::F32) => (
                "float".into(),
                float_constant((*value as f32) as f64),
                Type::Float(FloatType::F32),
            ),
            Constant::Bool(value) => (
                "i1".into(),
                if *value {
                    "true".into()
                } else {
                    "false".into()
                },
                Type::Boolean,
            ),
            Constant::Nothing => ("i8".into(), "0".into(), Type::Nothing),
            Constant::Str(text) => (
                "%sayon.str".into(),
                self.cg.str_constant(text),
                Type::String,
            ),
            Constant::NullPointer => ("ptr".into(), "null".into(), Type::Pointer),
        }
    }

    pub fn operand_type(&self, operand: &Operand) -> Type {
        match operand {
            Operand::Const(constant) => match constant {
                Constant::Int(_, int) => Type::Int(*int),
                Constant::Float(_, float) => Type::Float(*float),
                Constant::Bool(_) => Type::Boolean,
                Constant::Nothing => Type::Nothing,
                Constant::Str(_) => Type::String,
                Constant::NullPointer => Type::Pointer,
            },
            Operand::Copy(place) | Operand::Move(place) | Operand::Borrow { place, .. } => {
                self.place_type(place)
            }
        }
    }

    /// Clears the source of a move: the whole union when moving a payload.
    fn clear_moved(&mut self, place: &Place) {
        let target = match place.projection.last() {
            Some(Projection::Payload(_)) => Place {
                base: place.base.clone(),
                projection: place.projection[..place.projection.len() - 1].to_vec(),
            },
            _ => place.clone(),
        };
        let (pointer, ty) = self.place_ptr(&target);
        let llvm = self.cg.llvm(&ty);
        self.body
            .line(format!("store {llvm} zeroinitializer, ptr {pointer}"));
    }

    /// Loads an operand's value: (LLVM type, value, Sayon type).
    pub fn value(&mut self, operand: &Operand) -> (String, String, Type) {
        match operand {
            Operand::Const(constant) => self.const_value(constant),
            Operand::Copy(place) | Operand::Move(place) => {
                let (pointer, ty) = self.place_ptr(place);
                let llvm = self.cg.llvm(&ty);
                let loaded = self.body.tmp();
                self.body
                    .line(format!("{loaded} = load {llvm}, ptr {pointer}"));
                if matches!(operand, Operand::Move(_)) {
                    self.clear_moved(place);
                }
                (llvm, loaded, ty)
            }
            Operand::Borrow { place, .. } => {
                let (pointer, ty) = self.place_ptr(place);
                ("ptr".into(), pointer, ty)
            }
        }
    }

    /// A pointer to an operand's value for reading. Moved values are taken
    /// into a temporary that is destroyed after the current instruction.
    pub fn pointer(&mut self, operand: &Operand) -> (String, Type) {
        match operand {
            Operand::Borrow { place, .. } | Operand::Copy(place) => self.place_ptr(place),
            Operand::Move(_) | Operand::Const(_) => {
                let (llvm, value, ty) = self.value(operand);
                let temp = self.body.alloca(&llvm);
                self.body.line(format!("store {llvm} {value}, ptr {temp}"));
                if matches!(operand, Operand::Move(_)) && needs_drop(&ty) {
                    self.after.push((temp.clone(), ty.clone()));
                }
                (temp, ty)
            }
        }
    }

    fn finish_instruction(&mut self) {
        for (pointer, ty) in std::mem::take(&mut self.after) {
            let drop = self.cg.helper(Helper::Drop, &ty);
            self.body.line(format!("call void {drop}(ptr {pointer})"));
        }
    }

    pub fn drop_at(&mut self, pointer: &str, ty: &Type) {
        if needs_drop(ty) {
            let drop = self.cg.helper(Helper::Drop, ty);
            self.body.line(format!("call void {drop}(ptr {pointer})"));
        }
    }

    /// Stores an operand into `dest`, converting between union shapes.
    fn store_operand(&mut self, operand: &Operand, dest: &str, dest_ty: &Type) {
        let source_ty = self.operand_type(operand);
        if source_ty == *dest_ty
            || !(matches!(source_ty, Type::Union(_)) && matches!(dest_ty, Type::Union(_)))
        {
            let (llvm, value, _) = self.value(operand);
            self.body.line(format!("store {llvm} {value}, ptr {dest}"));
            return;
        }
        // Unions share tags and the payload offset; copy the common prefix.
        let dest_llvm = self.cg.llvm(dest_ty);
        self.body
            .line(format!("store {dest_llvm} zeroinitializer, ptr {dest}"));
        let bytes = self
            .cg
            .layouts
            .size_of(&source_ty)
            .min(self.cg.layouts.size_of(dest_ty));
        match operand {
            Operand::Copy(place) | Operand::Move(place) | Operand::Borrow { place, .. } => {
                let (source, _) = self.place_ptr(place);
                self.cg.intrinsic(MEMCPY.to_string());
                self.body.memcpy(dest, &source, &bytes.to_string());
                if matches!(operand, Operand::Move(_)) {
                    self.clear_moved(place);
                }
            }
            Operand::Const(_) => unreachable!("constants are never unions"),
        }
    }

    // ----- instructions --------------------------------------------------------------------

    fn inst(&mut self, inst: &Inst) {
        match inst {
            Inst::Assign { dest, value } => {
                let (pointer, ty) = self.place_ptr(dest);
                self.rvalue(value, &pointer, &ty);
            }
            Inst::Bind { slot, place, .. } => {
                let (pointer, _) = self.place_ptr(place);
                self.body
                    .line(format!("store ptr {pointer}, ptr %s{}", slot.0));
            }
            Inst::Drop(place) => {
                let ty = self.place_type(place);
                if needs_drop(&ty) {
                    let (pointer, _) = self.place_ptr(place);
                    self.drop_at(&pointer, &ty);
                }
            }
            Inst::Push { array, value } => {
                let (pointer, ty) = self.place_ptr(array);
                let Type::Array(element) = ty else {
                    panic!("push onto {ty}")
                };
                let size = self.cg.layouts.size_of(&element);
                self.body.line(format!(
                    "call void @sayon_array_reserve(ptr {pointer}, i64 1, i64 {size})"
                ));
                let data = self.body.tmp();
                self.body.line(format!("{data} = load ptr, ptr {pointer}"));
                let length_ptr = self.body.tmp();
                self.body.line(format!("{length_ptr} = getelementptr inbounds %sayon.array, ptr {pointer}, i32 0, i32 1"));
                let length = self.body.tmp();
                self.body
                    .line(format!("{length} = load i64, ptr {length_ptr}"));
                let element_llvm = self.cg.llvm(&element);
                let slot = self.body.tmp();
                self.body.line(format!(
                    "{slot} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {length}"
                ));
                self.store_operand(value, &slot, &element);
                let grown = self.body.tmp();
                self.body.line(format!("{grown} = add i64 {length}, 1"));
                self.body
                    .line(format!("store i64 {grown}, ptr {length_ptr}"));
            }
            Inst::DictSet { dict, key, value } => {
                let (pointer, ty) = self.place_ptr(dict);
                let Type::Dict(_, value_ty) = ty else {
                    panic!("dictionary assignment into {ty}")
                };
                let handle = self.body.tmp();
                self.body
                    .line(format!("{handle} = load ptr, ptr {pointer}"));
                let (key_ptr, _) = self.pointer(key);
                let value_llvm = self.cg.llvm(&value_ty);
                let temp = self.body.alloca(&value_llvm);
                self.store_operand(value, &temp, &value_ty);
                let drop = if needs_drop(&value_ty) {
                    self.cg.helper(Helper::Drop, &value_ty)
                } else {
                    "null".to_string()
                };
                self.body.line(format!(
                    "call void @sayon_dict_set(ptr {handle}, ptr {key_ptr}, ptr {temp}, ptr {drop})"
                ));
            }
        }
        self.finish_instruction();
    }

    fn rvalue(&mut self, rvalue: &Rvalue, dest: &str, dest_ty: &Type) {
        match rvalue {
            Rvalue::Use(operand) => self.store_operand(operand, dest, dest_ty),
            Rvalue::Not(operand) => {
                let (_, value, _) = self.value(operand);
                let result = self.body.tmp();
                self.body.line(format!("{result} = xor i1 {value}, true"));
                self.body.line(format!("store i1 {result}, ptr {dest}"));
            }
            Rvalue::FloatNegate(operand) => {
                let (llvm, value, _) = self.value(operand);
                let result = self.body.tmp();
                self.body.line(format!("{result} = fneg {llvm} {value}"));
                self.body.line(format!("store {llvm} {result}, ptr {dest}"));
            }
            Rvalue::FloatArith(op, lhs, rhs) => {
                let (llvm, a, _) = self.value(lhs);
                let (_, b, _) = self.value(rhs);
                let instruction = match op {
                    ArithOp::Add => "fadd",
                    ArithOp::Subtract => "fsub",
                    ArithOp::Multiply => "fmul",
                    ArithOp::Divide => "fdiv",
                    ArithOp::Remainder => "frem",
                };
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = {instruction} {llvm} {a}, {b}"));
                self.body.line(format!("store {llvm} {result}, ptr {dest}"));
            }
            Rvalue::Compare { op, ty, lhs, rhs } => {
                let result = self.compare(*op, ty, lhs, rhs);
                self.body.line(format!("store i1 {result}, ptr {dest}"));
            }
            Rvalue::Format(parts) => {
                let builder = self.body.tmp();
                self.body
                    .line(format!("{builder} = call ptr @sayon_builder_new()"));
                for part in parts {
                    match part {
                        FormatPart::Text(text) => self.builder_text(&builder, text),
                        FormatPart::Value(operand, _) => {
                            let (pointer, ty) = self.pointer(operand);
                            let display = self.cg.helper(Helper::Display, &ty);
                            self.body.line(format!(
                                "call void {display}(ptr {builder}, ptr {pointer}, i1 false)"
                            ));
                        }
                    }
                }
                self.body.line(format!(
                    "call void @sayon_builder_finish(ptr {builder}, ptr {dest})"
                ));
            }
            Rvalue::Concat(lhs, rhs) => {
                let (a, _) = self.pointer(lhs);
                let (b, _) = self.pointer(rhs);
                self.body.line(format!(
                    "call void @sayon_str_concat(ptr {a}, ptr {b}, ptr {dest})"
                ));
            }
            Rvalue::MakeArray { element, items } => {
                self.body
                    .line(format!("store %sayon.array zeroinitializer, ptr {dest}"));
                if items.is_empty() {
                    return;
                }
                let size = self.cg.layouts.size_of(element);
                self.body.line(format!(
                    "call void @sayon_array_reserve(ptr {dest}, i64 {}, i64 {size})",
                    items.len()
                ));
                let data = self.body.tmp();
                self.body.line(format!("{data} = load ptr, ptr {dest}"));
                let element_llvm = self.cg.llvm(element);
                for (index, item) in items.iter().enumerate() {
                    let slot = self.body.tmp();
                    self.body.line(format!(
                        "{slot} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {index}"
                    ));
                    self.store_operand(item, &slot, element);
                }
                let length_ptr = self.body.tmp();
                self.body.line(format!(
                    "{length_ptr} = getelementptr inbounds %sayon.array, ptr {dest}, i32 0, i32 1"
                ));
                self.body
                    .line(format!("store i64 {}, ptr {length_ptr}", items.len()));
            }
            Rvalue::MakeDict {
                key,
                value,
                entries,
            } => {
                let ops = self.cg.key_ops(key);
                self.body.line(format!(
                    "call void @sayon_dict_new(ptr {dest}, ptr {ops}, i64 {})",
                    self.cg.layouts.size_of(value)
                ));
                if entries.is_empty() {
                    return;
                }
                let handle = self.body.tmp();
                self.body.line(format!("{handle} = load ptr, ptr {dest}"));
                let value_llvm = self.cg.llvm(value);
                let drop = if needs_drop(value) {
                    self.cg.helper(Helper::Drop, value)
                } else {
                    "null".to_string()
                };
                for (key, item) in entries {
                    let (key_ptr, _) = self.pointer(key);
                    let temp = self.body.alloca(&value_llvm);
                    self.store_operand(item, &temp, value);
                    self.body.line(format!("call void @sayon_dict_set(ptr {handle}, ptr {key_ptr}, ptr {temp}, ptr {drop})"));
                    self.finish_instruction();
                }
            }
            Rvalue::MakeUnion { tag, value } => {
                let union_llvm = self.cg.llvm(dest_ty);
                self.body
                    .line(format!("store {union_llvm} zeroinitializer, ptr {dest}"));
                self.body.line(format!("store i32 {tag}, ptr {dest}"));
                let payload = self.body.tmp();
                self.body.line(format!(
                    "{payload} = getelementptr inbounds i8, ptr {dest}, i64 8"
                ));
                let member = self.operand_type(value);
                self.store_operand(value, &payload, &member);
            }
            Rvalue::MakeRecord { ty, fields } => {
                let Type::Named(named) = ty else {
                    panic!("record of {ty}")
                };
                let llvm = self.cg.llvm(ty);
                self.body
                    .line(format!("store {llvm} zeroinitializer, ptr {dest}"));
                let field_types =
                    sayon_check::types::structure_fields(&self.cg.module.types, named);
                for (index, (operand, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let offset = self.cg.layouts.field_offset(named, index as u32);
                    let target = self.offset_pointer(dest, offset);
                    self.store_operand(operand, &target, field_ty);
                }
            }
            Rvalue::MakeVariant {
                ty,
                variant,
                fields,
            } => {
                let Type::Named(named) = ty else {
                    panic!("variant of {ty}")
                };
                let llvm = self.cg.llvm(ty);
                self.body
                    .line(format!("store {llvm} zeroinitializer, ptr {dest}"));
                self.body
                    .line(format!("store i32 {}, ptr {dest}", variant + 1));
                let field_types =
                    sayon_check::types::variant_fields(&self.cg.module.types, named, *variant);
                for (index, (operand, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let offset =
                        self.cg
                            .layouts
                            .variant_field_offset(named, *variant, index as u32);
                    let target = self.offset_pointer(dest, offset);
                    self.store_operand(operand, &target, field_ty);
                }
            }
            Rvalue::VariantIs { value, variant } => {
                let (pointer, _) = self.pointer(value);
                let loaded = self.body.tmp();
                self.body
                    .line(format!("{loaded} = load i32, ptr {pointer}"));
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = icmp eq i32 {loaded}, {}", variant + 1));
                self.body.line(format!("store i1 {result}, ptr {dest}"));
            }
            Rvalue::TagIs { value, tag } => {
                let (pointer, _) = self.pointer(value);
                let loaded = self.body.tmp();
                self.body
                    .line(format!("{loaded} = load i32, ptr {pointer}"));
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = icmp eq i32 {loaded}, {tag}"));
                self.body.line(format!("store i1 {result}, ptr {dest}"));
            }
            Rvalue::UnionTypeName(value) => {
                let (pointer, _) = self.pointer(value);
                let tag = self.body.tmp();
                self.body.line(format!("{tag} = load i32, ptr {pointer}"));
                let index = self.body.tmp();
                self.body.line(format!("{index} = sub i32 {tag}, 1"));
                let wide = self.body.tmp();
                self.body.line(format!("{wide} = sext i32 {index} to i64"));
                let entry = self.body.tmp();
                self.body.line(format!(
                    "{entry} = getelementptr inbounds [{} x %sayon.str], ptr @sayon.type_names, i64 0, i64 {wide}",
                    self.cg.module.union_members.len()
                ));
                let name = self.body.tmp();
                self.body
                    .line(format!("{name} = load %sayon.str, ptr {entry}"));
                self.body
                    .line(format!("store %sayon.str {name}, ptr {dest}"));
            }
            Rvalue::Length { value, ty } => {
                let (pointer, _) = self.pointer(value);
                let result = self.body.tmp();
                match ty {
                    Type::String => self.body.line(format!(
                        "{result} = call i64 @sayon_str_chars(ptr {pointer})"
                    )),
                    // A view's length is where an array's is.
                    Type::Array(_) | Type::View(..) => {
                        let field = self.body.tmp();
                        self.body.line(format!("{field} = getelementptr inbounds %sayon.array, ptr {pointer}, i32 0, i32 1"));
                        self.body.line(format!("{result} = load i64, ptr {field}"));
                    }
                    _ => {
                        let handle = self.body.tmp();
                        self.body
                            .line(format!("{handle} = load ptr, ptr {pointer}"));
                        self.body
                            .line(format!("{result} = call i64 @sayon_dict_len(ptr {handle})"));
                    }
                }
                self.body.line(format!("store i64 {result}, ptr {dest}"));
            }
            Rvalue::ByteLength(value) => {
                let (pointer, _) = self.pointer(value);
                let field = self.body.tmp();
                self.body.line(format!(
                    "{field} = getelementptr inbounds %sayon.str, ptr {pointer}, i32 0, i32 1"
                ));
                let result = self.body.tmp();
                self.body.line(format!("{result} = load i64, ptr {field}"));
                self.body.line(format!("store i64 {result}, ptr {dest}"));
            }
            Rvalue::HasKey { dict, key } => {
                let (pointer, _) = self.pointer(dict);
                let handle = self.body.tmp();
                self.body
                    .line(format!("{handle} = load ptr, ptr {pointer}"));
                let (key_ptr, _) = self.pointer(key);
                let position = self.body.tmp();
                self.body.line(format!(
                    "{position} = call i64 @sayon_dict_find(ptr {handle}, ptr {key_ptr})"
                ));
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = icmp sge i64 {position}, 0"));
                self.body.line(format!("store i1 {result}, ptr {dest}"));
            }
            Rvalue::MakeShared { value, ty } => {
                let size = self.cg.layouts.size_of(ty);
                let block = self.body.tmp();
                self.body
                    .line(format!("{block} = call ptr @sayon_shared_new(i64 {size})"));
                let payload = self.offset_pointer(&block, SHARED_HEADER);
                self.store_operand(value, &payload, ty);
                self.body.line(format!("store ptr {block}, ptr {dest}"));
            }
            Rvalue::Downgrade(shared) => {
                let (pointer, _) = self.pointer(shared);
                let block = self.body.tmp();
                self.body.line(format!("{block} = load ptr, ptr {pointer}"));
                self.body
                    .line(format!("call void @sayon_weak_retain(ptr {block})"));
                self.body.line(format!("store ptr {block}, ptr {dest}"));
            }
            Rvalue::Upgrade {
                weak,
                shared_tag,
                nothing_tag,
            } => {
                let (pointer, _) = self.pointer(weak);
                let block = self.body.tmp();
                self.body.line(format!("{block} = load ptr, ptr {pointer}"));
                let alive = self.body.tmp();
                self.body.line(format!(
                    "{alive} = call zeroext i1 @sayon_shared_upgrade(ptr {block})"
                ));
                let union_llvm = self.cg.llvm(dest_ty);
                self.body
                    .line(format!("store {union_llvm} zeroinitializer, ptr {dest}"));
                let tag = self.body.tmp();
                self.body.line(format!(
                    "{tag} = select i1 {alive}, i32 {shared_tag}, i32 {nothing_tag}"
                ));
                self.body.line(format!("store i32 {tag}, ptr {dest}"));
                let handle = self.body.tmp();
                self.body.line(format!(
                    "{handle} = select i1 {alive}, ptr {block}, ptr null"
                ));
                let payload = self.offset_pointer(dest, 8);
                self.body.line(format!("store ptr {handle}, ptr {payload}"));
            }
            Rvalue::Clone {
                value,
                ty: Type::View(element, _),
            } => {
                // An array of copies of the viewed elements: the view has
                // the first two fields of an array, so copying it into an
                // array and cloning that array copies the elements.
                let (pointer, _) = self.pointer(value);
                let array = self.body.alloca("%sayon.array");
                self.body
                    .line(format!("store %sayon.array zeroinitializer, ptr {array}"));
                self.cg.intrinsic(MEMCPY.to_string());
                self.body.memcpy(&array, &pointer, "16");
                let array_ty = Type::array((**element).clone());
                let clone = self.cg.helper(Helper::Clone, &array_ty);
                self.body
                    .line(format!("call void {clone}(ptr {array}, ptr {dest})"));
            }
            Rvalue::Clone { value, ty } => {
                let (pointer, _) = self.pointer(value);
                let clone = self.cg.helper(Helper::Clone, ty);
                self.body
                    .line(format!("call void {clone}(ptr {pointer}, ptr {dest})"));
            }
            Rvalue::ErrorMessage(value) | Rvalue::ErrorCode(value) => {
                let (pointer, _) = self.pointer(value);
                let function = if matches!(rvalue, Rvalue::ErrorMessage(_)) {
                    "sayon_error_message"
                } else {
                    "sayon_error_code"
                };
                self.body
                    .line(format!("call void @{function}(ptr {pointer}, ptr {dest})"));
            }
            Rvalue::RangeLength(range) => {
                let (pointer, _) = self.pointer(range);
                let result = self.body.tmp();
                self.body.line(format!(
                    "{result} = call i64 @sayon_range_length(ptr {pointer})"
                ));
                self.body.line(format!("store i64 {result}, ptr {dest}"));
            }
            Rvalue::RangeAt { range, index } => {
                let (pointer, _) = self.pointer(range);
                let (_, k, _) = self.value(index);
                let start = self.body.tmp();
                self.body.line(format!("{start} = load i64, ptr {pointer}"));
                let step_ptr = self.body.tmp();
                self.body.line(format!(
                    "{step_ptr} = getelementptr inbounds %sayon.range, ptr {pointer}, i32 0, i32 2"
                ));
                let step = self.body.tmp();
                self.body.line(format!("{step} = load i64, ptr {step_ptr}"));
                let offset = self.body.tmp();
                self.body.line(format!("{offset} = mul i64 {k}, {step}"));
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = add i64 {start}, {offset}"));
                self.body.line(format!("store i64 {result}, ptr {dest}"));
            }
            Rvalue::Increment(value) => {
                let (_, v, _) = self.value(value);
                let result = self.body.tmp();
                self.body.line(format!("{result} = add i64 {v}, 1"));
                self.body.line(format!("store i64 {result}, ptr {dest}"));
            }
            Rvalue::RemoveKey {
                dict,
                key,
                value_tag,
                nothing_tag,
            } => {
                let dest_llvm = self.cg.llvm(dest_ty);
                self.body
                    .line(format!("store {dest_llvm} zeroinitializer, ptr {dest}"));
                let (pointer, _) = self.place_ptr(dict);
                let handle = self.body.tmp();
                self.body
                    .line(format!("{handle} = load ptr, ptr {pointer}"));
                let (key, _) = self.pointer(key);
                let out = if value_tag.is_some() {
                    self.offset_pointer(dest, 8)
                } else {
                    dest.to_string()
                };
                let found = self.body.tmp();
                self.body.line(format!(
                    "{found} = call zeroext i1 @sayon_dict_remove(ptr {handle}, ptr {key}, ptr {out})"
                ));
                let present = self.body.label("removed");
                let absent = self.body.label("absent");
                let done = self.body.label("remove_done");
                self.body
                    .line(format!("br i1 {found}, label %{present}, label %{absent}"));
                self.body.start(&present);
                if let Some(tag) = value_tag {
                    self.body.line(format!("store i32 {tag}, ptr {dest}"));
                }
                self.body.line(format!("br label %{done}"));
                self.body.start(&absent);
                if let Some(tag) = nothing_tag {
                    self.body.line(format!("store i32 {tag}, ptr {dest}"));
                }
                self.body.line(format!("br label %{done}"));
                self.body.start(&done);
            }
            Rvalue::NextChar { text, offset } => {
                let (text, _) = self.pointer(text);
                let (offset, _) = self.place_ptr(offset);
                self.body.line(format!(
                    "call void @sayon_str_next_char(ptr {text}, ptr {offset}, ptr {dest})"
                ));
            }
            Rvalue::MakeClosure {
                function,
                env,
                captures,
            } => {
                let environment = match env {
                    Some(env) => self.make_environment(env, captures),
                    None => "null".to_string(),
                };
                self.body
                    .line(format!("store ptr @sayon.fn.{}, ptr {dest}", function.0));
                let field = self.offset_pointer(dest, 8);
                self.body
                    .line(format!("store ptr {environment}, ptr {field}"));
            }
            Rvalue::Pop {
                array,
                element_tag,
                nothing_tag,
            } => self.pop(array, *element_tag, *nothing_tag, dest, dest_ty),
        }
    }

    fn pop(
        &mut self,
        array: &Place,
        element_tag: Option<u32>,
        nothing_tag: Option<u32>,
        dest: &str,
        dest_ty: &Type,
    ) {
        let (pointer, ty) = self.place_ptr(array);
        let Type::Array(element) = ty else {
            panic!("pop from {ty}")
        };
        let dest_llvm = self.cg.llvm(dest_ty);
        self.body
            .line(format!("store {dest_llvm} zeroinitializer, ptr {dest}"));
        let length_ptr = self.body.tmp();
        self.body.line(format!(
            "{length_ptr} = getelementptr inbounds %sayon.array, ptr {pointer}, i32 0, i32 1"
        ));
        let length = self.body.tmp();
        self.body
            .line(format!("{length} = load i64, ptr {length_ptr}"));
        let empty = self.body.tmp();
        self.body.line(format!("{empty} = icmp eq i64 {length}, 0"));
        let when_empty = self.body.label("pop_empty");
        let when_some = self.body.label("pop_some");
        let done = self.body.label("pop_done");
        self.body.line(format!(
            "br i1 {empty}, label %{when_empty}, label %{when_some}"
        ));

        self.body.start(&when_empty);
        if let Some(tag) = nothing_tag {
            self.body.line(format!("store i32 {tag}, ptr {dest}"));
        }
        self.body.line(format!("br label %{done}"));

        self.body.start(&when_some);
        let last = self.body.tmp();
        self.body.line(format!("{last} = sub i64 {length}, 1"));
        self.body
            .line(format!("store i64 {last}, ptr {length_ptr}"));
        let data = self.body.tmp();
        self.body.line(format!("{data} = load ptr, ptr {pointer}"));
        let element_llvm = self.cg.llvm(&element);
        let source = self.body.tmp();
        self.body.line(format!(
            "{source} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {last}"
        ));
        let target = match element_tag {
            Some(tag) => {
                self.body.line(format!("store i32 {tag}, ptr {dest}"));
                let payload = self.body.tmp();
                self.body.line(format!(
                    "{payload} = getelementptr inbounds i8, ptr {dest}, i64 8"
                ));
                payload
            }
            None => dest.to_string(),
        };
        let bytes = self
            .cg
            .layouts
            .size_of(&element)
            .min(
                self.cg
                    .layouts
                    .size_of(dest_ty)
                    .max(if element_tag.is_some() {
                        self.cg.layouts.size_of(&element)
                    } else {
                        0
                    }),
            );
        self.cg.intrinsic(MEMCPY.to_string());
        self.body.memcpy(&target, &source, &bytes.to_string());
        self.body.line(format!("br label %{done}"));
        self.body.start(&done);
    }

    /// Allocates a closure environment holding the captured values.
    fn make_environment(&mut self, env: &Type, captures: &[Operand]) -> String {
        let Type::Named(named) = env else {
            panic!("environment of type {env}")
        };
        let size = 16 + self.cg.layouts.size_of(env);
        let block = self.body.tmp();
        self.body
            .line(format!("{block} = call ptr @sayon_alloc(i64 {size})"));
        let drop = self.cg.helper(Helper::EnvDrop, env);
        let clone = self.cg.helper(Helper::EnvClone, env);
        self.body.line(format!("store ptr {drop}, ptr {block}"));
        let clone_field = self.offset_pointer(&block, 8);
        self.body
            .line(format!("store ptr {clone}, ptr {clone_field}"));
        let fields = sayon_check::types::structure_fields(&self.cg.module.types, named);
        for (index, (capture, field_ty)) in captures.iter().zip(&fields).enumerate() {
            let offset = 16 + self.cg.layouts.field_offset(named, index as u32);
            let address = self.offset_pointer(&block, offset);
            self.store_operand(capture, &address, field_ty);
        }
        block
    }

    fn builder_text(&mut self, builder: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let constant = self.cg.bytes(text.as_bytes());
        self.body.line(format!(
            "call void @sayon_builder_bytes(ptr {builder}, ptr {constant}, i64 {})",
            text.len()
        ));
    }

    fn compare(&mut self, op: CompareOp, ty: &Type, lhs: &Operand, rhs: &Operand) -> String {
        let result = self.body.tmp();
        match ty {
            Type::Int(int) => {
                let (llvm, a, _) = self.value(lhs);
                let (_, b, _) = self.value(rhs);
                let predicate = match (op, int.signed()) {
                    (CompareOp::Equal, _) => "eq",
                    (CompareOp::NotEqual, _) => "ne",
                    (CompareOp::Less, true) => "slt",
                    (CompareOp::Greater, true) => "sgt",
                    (CompareOp::LessEqual, true) => "sle",
                    (CompareOp::GreaterEqual, true) => "sge",
                    (CompareOp::Less, false) => "ult",
                    (CompareOp::Greater, false) => "ugt",
                    (CompareOp::LessEqual, false) => "ule",
                    (CompareOp::GreaterEqual, false) => "uge",
                };
                self.body
                    .line(format!("{result} = icmp {predicate} {llvm} {a}, {b}"));
            }
            Type::Float(_) => {
                let (llvm, a, _) = self.value(lhs);
                let (_, b, _) = self.value(rhs);
                let predicate = match op {
                    CompareOp::Equal => "oeq",
                    CompareOp::NotEqual => "une",
                    CompareOp::Less => "olt",
                    CompareOp::Greater => "ogt",
                    CompareOp::LessEqual => "ole",
                    CompareOp::GreaterEqual => "oge",
                };
                self.body
                    .line(format!("{result} = fcmp {predicate} {llvm} {a}, {b}"));
            }
            Type::Boolean | Type::Nothing | Type::Pointer => {
                let (llvm, a, _) = self.value(lhs);
                let (_, b, _) = self.value(rhs);
                let predicate = if op == CompareOp::NotEqual {
                    "ne"
                } else {
                    "eq"
                };
                self.body
                    .line(format!("{result} = icmp {predicate} {llvm} {a}, {b}"));
            }
            Type::String if !matches!(op, CompareOp::Equal | CompareOp::NotEqual) => {
                let (a, _) = self.pointer(lhs);
                let (b, _) = self.pointer(rhs);
                let order = self.body.tmp();
                self.body.line(format!(
                    "{order} = call i32 @sayon_str_cmp(ptr {a}, ptr {b})"
                ));
                let predicate = match op {
                    CompareOp::Less => "slt",
                    CompareOp::Greater => "sgt",
                    CompareOp::LessEqual => "sle",
                    _ => "sge",
                };
                self.body
                    .line(format!("{result} = icmp {predicate} i32 {order}, 0"));
            }
            _ => {
                let (a, _) = self.pointer(lhs);
                let (b, _) = self.pointer(rhs);
                let equal = self.body.tmp();
                if *ty == Type::String {
                    self.body.line(format!(
                        "{equal} = call zeroext i1 @sayon_str_eq(ptr {a}, ptr {b})"
                    ));
                } else {
                    let eq = self.cg.helper(Helper::Eq, ty);
                    self.body
                        .line(format!("{equal} = call zeroext i1 {eq}(ptr {a}, ptr {b})"));
                }
                if op == CompareOp::NotEqual {
                    self.body.line(format!("{result} = xor i1 {equal}, true"));
                } else {
                    return equal;
                }
            }
        }
        result
    }

    // ----- terminators ---------------------------------------------------------------------------

    fn terminator(&mut self, term: &Terminator) {
        match term {
            Terminator::Goto(target) => self.body.line(format!("br label %b{}", target.0)),
            Terminator::Branch {
                condition,
                then_block,
                else_block,
            } => {
                let (_, value, _) = self.value(condition);
                self.body.line(format!(
                    "br i1 {value}, label %b{}, label %b{}",
                    then_block.0, else_block.0
                ));
            }
            Terminator::Return => {
                let ret_llvm = self.cg.llvm(&self.f.ret);
                let value = self.body.tmp();
                self.body.line(format!(
                    "{value} = load {ret_llvm}, ptr %s{}",
                    self.f.ret_slot.0
                ));
                self.body
                    .line(format!("store {ret_llvm} {value}, ptr %ret"));
                self.body.line("ret i32 0");
            }
            Terminator::ReturnError => {
                let value = self.body.tmp();
                self.body
                    .line(format!("{value} = load ptr, ptr {}", self.error_slot()));
                self.body.line(format!("store ptr {value}, ptr %err"));
                self.body.line("ret i32 1");
            }
            Terminator::Unreachable => self.body.line("unreachable"),
            Terminator::Op {
                op,
                dest,
                normal,
                error,
                span,
            } => {
                let status = self.op(op, dest.as_ref(), *span, *error);
                self.finish_instruction();
                match status {
                    Some(status) => {
                        let ok = self.body.tmp();
                        self.body.line(format!("{ok} = icmp eq i32 {status}, 0"));
                        self.body.line(format!(
                            "br i1 {ok}, label %b{}, label %b{}",
                            normal.0, error.0
                        ));
                    }
                    None => self.body.line(format!("br label %b{}", normal.0)),
                }
            }
        }
    }

    /// Emits a failing operation and returns its status value (0 on success),
    /// or `None` when it branched to the error block itself.
    fn op(&mut self, op: &Op, dest: Option<&Place>, span: Span, error: BlockId) -> Option<String> {
        let err = self.error_slot();
        match op {
            Op::Spawn { body, result } => {
                // The task takes the function value over.
                let (llvm, value, _) = self.value(body);
                let function = self.body.alloca(&llvm);
                self.body
                    .line(format!("store {llvm} {value}, ptr {function}"));
                let size = self.cg.layouts.size_of(result);
                let drop = if needs_drop(result) {
                    self.cg.helper(Helper::Drop, result)
                } else {
                    "null".to_string()
                };
                let out = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("ptr"),
                };
                self.body.line(format!(
                    "call void @sayon_task_spawn(ptr {function}, i64 {size}, ptr {drop}, ptr {out})"
                ));
                Some("0".to_string())
            }
            Op::Join { task } => {
                let (task, _) = self.place_ptr(task);
                let out = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("i64"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_task_join(ptr {err}, ptr {task}, ptr {out}, ptr {location})"
                ));
                Some(status)
            }
            Op::Detach { task } => {
                let (task, _) = self.place_ptr(task);
                self.body
                    .line(format!("call void @sayon_task_detach(ptr {task})"));
                Some("0".to_string())
            }
            Op::Yield => {
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_task_yield(ptr {err}, ptr {location})"
                ));
                Some(status)
            }
            Op::Sleep { nanoseconds } => {
                let (_, nanoseconds, _) = self.value(nanoseconds);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_task_sleep(ptr {err}, i64 {nanoseconds}, ptr {location})"
                ));
                Some(status)
            }
            Op::ChannelNew { capacity, element } => {
                let (_, capacity, _) = self.value(capacity);
                let size = self.cg.layouts.size_of(element);
                let drop = if needs_drop(element) {
                    self.cg.helper(Helper::Drop, element)
                } else {
                    "null".to_string()
                };
                let out = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("ptr"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_channel_new(ptr {err}, i64 {capacity}, i64 {size}, ptr {drop}, ptr {out}, ptr {location})"
                ));
                Some(status)
            }
            Op::Send {
                channel,
                message,
                keep,
            } => {
                let (channel, _) = self.pointer(channel);
                let (message, _) = self.place_ptr(message);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_channel_send(ptr {err}, ptr {channel}, ptr {message}, i1 {keep}, ptr {location})"
                ));
                Some(status)
            }
            Op::Receive { channel } => {
                let (channel, channel_ty) = self.pointer(channel);
                let out = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => {
                        let Type::Wrapper(_, element) = &channel_ty else {
                            panic!("receive from {channel_ty}")
                        };
                        let llvm = self.cg.llvm(element);
                        self.body.alloca(&llvm)
                    }
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_channel_receive(ptr {err}, ptr {channel}, ptr {out}, ptr {location})"
                ));
                Some(status)
            }
            Op::Close { channel } => {
                let (channel, _) = self.pointer(channel);
                self.body
                    .line(format!("call void @sayon_channel_close(ptr {channel})"));
                Some("0".to_string())
            }
            Op::Select { channels } => {
                let (channels, _) = self.pointer(channels);
                let out = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("i64"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_channel_select(ptr {err}, ptr {channels}, ptr {out}, ptr {location})"
                ));
                Some(status)
            }
            Op::Call { function, args } => {
                let callee = self.cg.module.function(*function);
                let mut values = Vec::new();
                for (index, slot) in callee.params.iter().enumerate() {
                    let definition = callee.slot(*slot);
                    let reference = matches!(definition.kind, SlotKind::Ref { .. });
                    let param_llvm = if reference {
                        "ptr".to_string()
                    } else {
                        self.cg.llvm(&definition.ty)
                    };
                    match args.get(index) {
                        Some(arg) if reference => {
                            let (pointer, _) = self.pointer(arg);
                            values.push(format!("ptr {pointer}"));
                        }
                        Some(arg) => {
                            let temp = self.body.alloca(&param_llvm);
                            self.store_operand(arg, &temp, &definition.ty);
                            let loaded = self.body.tmp();
                            self.body
                                .line(format!("{loaded} = load {param_llvm}, ptr {temp}"));
                            values.push(format!("{param_llvm} {loaded}"));
                        }
                        None if reference => values.push("ptr null".to_string()),
                        None => values.push(format!("{param_llvm} zeroinitializer")),
                    }
                }
                let ret_pointer = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => {
                        let ret_llvm = self.cg.llvm(&callee.ret);
                        self.body.alloca(&ret_llvm)
                    }
                };
                let status = self.body.tmp();
                let mut all = vec![
                    format!("ptr {ret_pointer}"),
                    format!("ptr {err}"),
                    format!("i64 {}", args.len()),
                ];
                all.extend(values);
                self.body.line(format!(
                    "{status} = call i32 @sayon.fn.{}({})",
                    function.0,
                    all.join(", ")
                ));
                Some(status)
            }
            Op::CallValue { callee, args } => {
                let (pointer, ty) = self.place_ptr(callee);
                let Type::Function(signature) = ty else {
                    panic!("call of a {ty} value")
                };
                let code = self.body.tmp();
                self.body.line(format!("{code} = load ptr, ptr {pointer}"));
                let field = self.offset_pointer(&pointer, 8);
                let block = self.body.tmp();
                self.body.line(format!("{block} = load ptr, ptr {field}"));
                // A named function's value has no environment; its code
                // never reads the pointer.
                let environment = self.body.tmp();
                self.body.line(format!(
                    "{environment} = getelementptr i8, ptr {block}, i64 16"
                ));
                let mut values = vec![format!("ptr {environment}")];
                for (arg, (mode, param_ty)) in args.iter().zip(&signature.params) {
                    if by_reference(*mode, param_ty) {
                        let (pointer, _) = self.pointer(arg);
                        values.push(format!("ptr {pointer}"));
                    } else {
                        let param_llvm = self.cg.llvm(param_ty);
                        let temp = self.body.alloca(&param_llvm);
                        self.store_operand(arg, &temp, param_ty);
                        let loaded = self.body.tmp();
                        self.body
                            .line(format!("{loaded} = load {param_llvm}, ptr {temp}"));
                        values.push(format!("{param_llvm} {loaded}"));
                    }
                }
                let ret_pointer = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => {
                        let ret_llvm = self.cg.llvm(&signature.ret);
                        self.body.alloca(&ret_llvm)
                    }
                };
                let status = self.body.tmp();
                let mut all = vec![
                    format!("ptr {ret_pointer}"),
                    format!("ptr {err}"),
                    format!("i64 {}", values.len()),
                ];
                all.extend(values);
                self.body
                    .line(format!("{status} = call i32 {code}({})", all.join(", ")));
                Some(status)
            }
            Op::IntArith { op, ty, lhs, rhs } => {
                Some(self.int_arith(*op, *ty, lhs, rhs, dest, span, error))
            }
            Op::IntNegate { ty, value } => {
                let llvm = int_llvm(*ty);
                let (_, a, _) = self.value(value);
                let declaration = format!(
                    "declare {{ {llvm}, i1 }} @llvm.ssub.with.overflow.{llvm}({llvm}, {llvm})"
                );
                self.cg.intrinsic(declaration);
                let pair = self.body.tmp();
                self.body.line(format!("{pair} = call {{ {llvm}, i1 }} @llvm.ssub.with.overflow.{llvm}({llvm} 0, {llvm} {a})"));
                let result = self.body.tmp();
                self.body.line(format!(
                    "{result} = extractvalue {{ {llvm}, i1 }} {pair}, 0"
                ));
                let overflow = self.body.tmp();
                self.body.line(format!(
                    "{overflow} = extractvalue {{ {llvm}, i1 }} {pair}, 1"
                ));
                let fail = self.body.label("negate_overflow");
                let ok = self.body.label("negate_ok");
                self.body
                    .line(format!("br i1 {overflow}, label %{fail}, label %{ok}"));
                self.body.start(&fail);
                let wide = self.widen(*ty, &a);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_error_negate(ptr {err}, i64 {wide}, i32 {}, ptr {location})",
                    int_code(*ty)
                ));
                self.body.line(format!("br label %b{}", error.0));
                self.body.start(&ok);
                if let Some(place) = dest {
                    let (pointer, _) = self.place_ptr(place);
                    self.body
                        .line(format!("store {llvm} {result}, ptr {pointer}"));
                }
                Some("0".to_string())
            }
            Op::Convert { from, to, value } => Some(self.convert(from, to, value, dest, span)),
            Op::CheckIndex { array, index } => {
                let (pointer, _) = self.pointer(array);
                let field = self.body.tmp();
                self.body.line(format!(
                    "{field} = getelementptr inbounds %sayon.array, ptr {pointer}, i32 0, i32 1"
                ));
                let length = self.body.tmp();
                self.body.line(format!("{length} = load i64, ptr {field}"));
                let (_, position, _) = self.value(index);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_check_index(ptr {err}, i64 {position}, i64 {length}, ptr {location})"
                ));
                if let Some(place) = dest {
                    let (target, _) = self.place_ptr(place);
                    self.body
                        .line(format!("store i64 {position}, ptr {target}"));
                }
                Some(status)
            }
            Op::MakeView { base, start, end } => {
                let (pointer, ty) = self.place_ptr(base);
                // A string starts like an array of its UTF-8 bytes.
                let element = match ty {
                    Type::Array(element) | Type::View(element, _) => *element,
                    Type::String => Type::Int(IntType::U8),
                    other => panic!("view of {other}"),
                };
                let data = self.body.tmp();
                self.body.line(format!("{data} = load ptr, ptr {pointer}"));
                let length_field = self.offset_pointer(&pointer, 8);
                let length = self.body.tmp();
                self.body
                    .line(format!("{length} = load i64, ptr {length_field}"));
                let start = match start {
                    Some(start) => self.value(start).1,
                    None => "0".to_string(),
                };
                let end = match end {
                    Some(end) => self.value(end).1,
                    None => length.clone(),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_check_view(ptr {err}, i64 {start}, i64 {end}, i64 {length}, ptr {location})"
                ));
                let element_llvm = self.cg.llvm(&element);
                let first = self.body.tmp();
                self.body.line(format!(
                    "{first} = getelementptr {element_llvm}, ptr {data}, i64 {start}"
                ));
                let count = self.body.tmp();
                self.body.line(format!("{count} = sub i64 {end}, {start}"));
                if let Some(place) = dest {
                    let (target, _) = self.place_ptr(place);
                    self.body.line(format!("store ptr {first}, ptr {target}"));
                    let target_length = self.offset_pointer(&target, 8);
                    self.body
                        .line(format!("store i64 {count}, ptr {target_length}"));
                }
                Some(status)
            }
            Op::SplitView { view, count } => {
                let (pointer, ty) = self.place_ptr(view);
                let Type::View(element, _) = ty else {
                    panic!("split of {ty}")
                };
                let data = self.body.tmp();
                self.body.line(format!("{data} = load ptr, ptr {pointer}"));
                let length_field = self.offset_pointer(&pointer, 8);
                let length = self.body.tmp();
                self.body
                    .line(format!("{length} = load i64, ptr {length_field}"));
                let (_, count, _) = self.value(count);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_check_split(ptr {err}, i64 {count}, i64 {length}, ptr {location})"
                ));
                // On failure the view is left as it was.
                let ok = self.body.tmp();
                self.body.line(format!("{ok} = icmp eq i32 {status}, 0"));
                let taken = self.body.tmp();
                self.body
                    .line(format!("{taken} = select i1 {ok}, i64 {count}, i64 0"));
                let element_llvm = self.cg.llvm(&element);
                let rest = self.body.tmp();
                self.body.line(format!(
                    "{rest} = getelementptr {element_llvm}, ptr {data}, i64 {taken}"
                ));
                let remaining = self.body.tmp();
                self.body
                    .line(format!("{remaining} = sub i64 {length}, {taken}"));
                if let Some(place) = dest {
                    let (target, _) = self.place_ptr(place);
                    self.body.line(format!("store ptr {data}, ptr {target}"));
                    let target_length = self.offset_pointer(&target, 8);
                    self.body
                        .line(format!("store i64 {taken}, ptr {target_length}"));
                }
                self.body.line(format!("store ptr {rest}, ptr {pointer}"));
                self.body
                    .line(format!("store i64 {remaining}, ptr {length_field}"));
                Some(status)
            }
            Op::Swap {
                array,
                first,
                second,
            } => {
                let (pointer, ty) = self.place_ptr(array);
                let Type::Array(element) = ty else {
                    panic!("swap in {ty}")
                };
                let size = self.cg.layouts.size_of(&element);
                let (_, first, _) = self.value(first);
                let (_, second, _) = self.value(second);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_array_swap(ptr {err}, ptr {pointer}, i64 {first}, i64 {second}, i64 {size}, ptr {location})"
                ));
                Some(status)
            }
            Op::RemoveAt { array, index } => {
                let (pointer, ty) = self.place_ptr(array);
                let Type::Array(element) = ty else {
                    panic!("remove from {ty}")
                };
                let size = self.cg.layouts.size_of(&element);
                let (_, position, _) = self.value(index);
                let target = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => {
                        let llvm = self.cg.llvm(&element);
                        self.body.alloca(&llvm)
                    }
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_array_remove(ptr {err}, ptr {pointer}, i64 {position}, i64 {size}, ptr {target}, ptr {location})"
                ));
                Some(status)
            }
            Op::CharAt { text, index } => {
                let (text, _) = self.pointer(text);
                let (_, position, _) = self.value(index);
                let target = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("%sayon.str"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_str_char_at(ptr {err}, ptr {text}, i64 {position}, ptr {target}, ptr {location})"
                ));
                Some(status)
            }
            Op::DictFind { dict, key } => {
                let (pointer, _) = self.pointer(dict);
                let handle = self.body.tmp();
                self.body
                    .line(format!("{handle} = load ptr, ptr {pointer}"));
                let (key_ptr, _) = self.pointer(key);
                let target = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("i64"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_dict_find_checked(ptr {err}, ptr {handle}, ptr {key_ptr}, ptr {location}, ptr {target})"
                ));
                Some(status)
            }
            Op::Print(values) => {
                let builder = self.body.tmp();
                self.body
                    .line(format!("{builder} = call ptr @sayon_builder_new()"));
                for (index, (operand, _)) in values.iter().enumerate() {
                    if index > 0 {
                        self.builder_text(&builder, " ");
                    }
                    let (pointer, ty) = self.pointer(operand);
                    let display = self.cg.helper(Helper::Display, &ty);
                    self.body.line(format!(
                        "call void {display}(ptr {builder}, ptr {pointer}, i1 false)"
                    ));
                }
                let line = self.body.alloca("%sayon.str");
                self.body.line(format!(
                    "call void @sayon_builder_finish(ptr {builder}, ptr {line})"
                ));
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_print(ptr {err}, ptr {line}, ptr {location})"
                ));
                self.body
                    .line(format!("call void @sayon_str_drop(ptr {line})"));
                Some(status)
            }
            Op::Assert { condition, message } => {
                let (_, holds, _) = self.value(condition);
                let fail = self.body.label("assert_failed");
                let ok = self.body.label("assert_ok");
                self.body
                    .line(format!("br i1 {holds}, label %{ok}, label %{fail}"));
                self.body.start(&fail);
                let text = match message {
                    Some(message) => self.pointer(message).0,
                    None => "null".to_string(),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!("{status} = call i32 @sayon_assert_failed(ptr {err}, ptr {text}, ptr {location})"));
                self.finish_instruction();
                self.body.line(format!("br label %b{}", error.0));
                self.body.start(&ok);
                Some("0".to_string())
            }
            Op::MakeRange { start, end, step } => {
                let (_, s, _) = self.value(start);
                let (_, e, _) = self.value(end);
                let step = match step {
                    Some(step) => self.value(step).1,
                    None => "1".to_string(),
                };
                let target = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("%sayon.range"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_range_new(ptr {err}, i64 {s}, i64 {e}, i64 {step}, ptr {location}, ptr {target})"
                ));
                Some(status)
            }
            Op::ToNumber {
                text,
                integer_tag,
                float_tag,
            } => {
                let (pointer, _) = self.pointer(text);
                let is_float = self.body.alloca("i1");
                let integer = self.body.alloca("i64");
                let float = self.body.alloca("double");
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_tonumber(ptr {err}, ptr {pointer}, ptr {location}, ptr {is_float}, ptr {integer}, ptr {float})"
                ));
                self.finish_instruction();
                let ok = self.body.tmp();
                self.body.line(format!("{ok} = icmp eq i32 {status}, 0"));
                let store = self.body.label("number_ok");
                self.body
                    .line(format!("br i1 {ok}, label %{store}, label %b{}", error.0));
                self.body.start(&store);
                if let Some(place) = dest {
                    let (target, ty) = self.place_ptr(place);
                    let union_llvm = self.cg.llvm(&ty);
                    self.body
                        .line(format!("store {union_llvm} zeroinitializer, ptr {target}"));
                    let flag = self.body.tmp();
                    self.body.line(format!("{flag} = load i1, ptr {is_float}"));
                    let tag = self.body.tmp();
                    self.body.line(format!(
                        "{tag} = select i1 {flag}, i32 {float_tag}, i32 {integer_tag}"
                    ));
                    self.body.line(format!("store i32 {tag}, ptr {target}"));
                    let payload = self.body.tmp();
                    self.body.line(format!(
                        "{payload} = getelementptr inbounds i8, ptr {target}, i64 8"
                    ));
                    let as_float = self.body.tmp();
                    self.body
                        .line(format!("{as_float} = load i64, ptr {float}"));
                    let as_integer = self.body.tmp();
                    self.body
                        .line(format!("{as_integer} = load i64, ptr {integer}"));
                    let bits = self.body.tmp();
                    self.body.line(format!(
                        "{bits} = select i1 {flag}, i64 {as_float}, i64 {as_integer}"
                    ));
                    self.body.line(format!("store i64 {bits}, ptr {payload}"));
                }
                Some("0".to_string())
            }
            Op::ReadFile(path) => {
                let (pointer, _) = self.pointer(path);
                let target = match dest {
                    Some(place) => self.place_ptr(place).0,
                    None => self.body.alloca("%sayon.str"),
                };
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_read_file(ptr {err}, ptr {pointer}, ptr {location}, ptr {target})"
                ));
                Some(status)
            }
            Op::WriteFile { path, content } => {
                let (path, _) = self.pointer(path);
                let (content, _) = self.pointer(content);
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_write_file(ptr {err}, ptr {path}, ptr {content}, ptr {location})"
                ));
                Some(status)
            }
            Op::Fail(message) => {
                let (pointer, _) = self.pointer(message);
                let code = self.cg.c_string("UserError");
                let location = self.cg.location(span);
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_error_raise(ptr {err}, ptr {code}, ptr {pointer}, ptr {location})"
                ));
                Some(status)
            }
            Op::Host {
                function,
                args,
                tags,
            } => {
                let count = args.len().max(1);
                let argv = self.body.alloca(&format!("[{count} x ptr]"));
                for (index, arg) in args.iter().enumerate() {
                    let (pointer, _) = self.pointer(arg);
                    let entry = self.body.tmp();
                    self.body.line(format!(
                        "{entry} = getelementptr inbounds [{count} x ptr], ptr {argv}, i64 0, i64 {index}"
                    ));
                    self.body.line(format!("store ptr {pointer}, ptr {entry}"));
                }
                let location = self.cg.location(span);
                let code = *function as u32;
                match tags {
                    Some((string_tag, nothing_tag)) => {
                        let text = self.body.alloca("%sayon.str");
                        self.body
                            .line(format!("store %sayon.str zeroinitializer, ptr {text}"));
                        let present = self.body.alloca("i1");
                        self.body.line(format!("store i1 false, ptr {present}"));
                        let status = self.body.tmp();
                        self.body.line(format!(
                            "{status} = call i32 @sayon_host_call(ptr {err}, i32 {code}, ptr {argv}, ptr {text}, ptr {present}, ptr {location})"
                        ));
                        self.finish_instruction();
                        let ok = self.body.tmp();
                        self.body.line(format!("{ok} = icmp eq i32 {status}, 0"));
                        let build = self.body.label("host_ok");
                        self.body
                            .line(format!("br i1 {ok}, label %{build}, label %b{}", error.0));
                        self.body.start(&build);
                        if let Some(place) = dest {
                            let (target, ty) = self.place_ptr(place);
                            let union_llvm = self.cg.llvm(&ty);
                            self.body
                                .line(format!("store {union_llvm} zeroinitializer, ptr {target}"));
                            let flag = self.body.tmp();
                            self.body.line(format!("{flag} = load i1, ptr {present}"));
                            let tag = self.body.tmp();
                            self.body.line(format!(
                                "{tag} = select i1 {flag}, i32 {string_tag}, i32 {nothing_tag}"
                            ));
                            self.body.line(format!("store i32 {tag}, ptr {target}"));
                            let payload = self.body.tmp();
                            self.body.line(format!(
                                "{payload} = getelementptr inbounds i8, ptr {target}, i64 8"
                            ));
                            let value = self.body.tmp();
                            self.body
                                .line(format!("{value} = load %sayon.str, ptr {text}"));
                            self.body
                                .line(format!("store %sayon.str {value}, ptr {payload}"));
                        }
                        Some("0".to_string())
                    }
                    None => {
                        let out = match dest {
                            Some(place) => self.place_ptr(place).0,
                            None => self.body.alloca("%sayon.array"),
                        };
                        let status = self.body.tmp();
                        self.body.line(format!(
                            "{status} = call i32 @sayon_host_call(ptr {err}, i32 {code}, ptr {argv}, ptr {out}, ptr null, ptr {location})"
                        ));
                        Some(status)
                    }
                }
            }
            Op::Raise(problem) => {
                let (_, value, _) = self.value(problem);
                self.body
                    .line(format!("call void @sayon_error_drop(ptr {err})"));
                self.body.line(format!("store ptr {value}, ptr {err}"));
                Some("1".to_string())
            }
            Op::CallForeign { function, args } => {
                let declaration = &self.cg.module.foreign[function.0 as usize];
                let (name, library, ret) = (
                    declaration.name.clone(),
                    declaration.library.clone(),
                    declaration.ret.clone(),
                );
                let mut params: Vec<String> = declaration.params.iter().map(c_param).collect();
                if declaration.variadic {
                    params.push("...".to_string());
                }
                let mut values = Vec::new();
                for (operand, ty) in args {
                    let value = if *ty == Type::CString {
                        let (slot, _) = self.pointer(operand);
                        let text = self.body.tmp();
                        self.body.line(format!("{text} = load ptr, ptr {slot}"));
                        text
                    } else {
                        self.value(operand).1
                    };
                    values.push(format!("{} {value}", c_param(ty)));
                }
                let signature = format!("({})", params.join(", "));
                let call = format!(
                    "call {} {signature} @{name}({})",
                    c_llvm(&ret),
                    values.join(", ")
                );
                if ret == Type::Nothing {
                    self.body.line(call);
                    if let Some(place) = dest {
                        let (target, _) = self.place_ptr(place);
                        self.body.line(format!("store i8 0, ptr {target}"));
                    }
                } else {
                    let result = self.body.tmp();
                    self.body.line(format!("{result} = {call}"));
                    if let Some(place) = dest {
                        let (target, _) = self.place_ptr(place);
                        self.body
                            .line(format!("store {} {result}, ptr {target}", c_llvm(&ret)));
                    }
                }
                // What C prints comes out in order with what the program
                // prints.
                if library == "c" {
                    self.body.line("call i32 (ptr) @fflush(ptr null)");
                }
                self.finish_instruction();
                Some("0".to_string())
            }
            Op::RawRead {
                address,
                offset,
                ty,
            } => {
                let at = self.raw_address(address, offset, "read from", span, error);
                let llvm = self.cg.llvm(ty);
                let value = self.body.tmp();
                self.body
                    .line(format!("{value} = load {llvm}, ptr {at}, align 1"));
                if let Some(place) = dest {
                    let (target, _) = self.place_ptr(place);
                    self.body
                        .line(format!("store {llvm} {value}, ptr {target}"));
                }
                Some("0".to_string())
            }
            Op::RawWrite {
                address,
                offset,
                value,
                ty,
            } => {
                let at = self.raw_address(address, offset, "write to", span, error);
                let llvm = self.cg.llvm(ty);
                let (_, value, _) = self.value(value);
                self.body
                    .line(format!("store {llvm} {value}, ptr {at}, align 1"));
                Some("0".to_string())
            }
        }
    }

    /// The address `offset` bytes after a pointer; fails, branching to
    /// `error`, for `null_pointer`.
    fn raw_address(
        &mut self,
        address: &Operand,
        offset: &Operand,
        action: &str,
        span: Span,
        error: BlockId,
    ) -> String {
        let err = self.error_slot();
        let (_, base, _) = self.value(address);
        let (_, offset, _) = self.value(offset);
        let null = self.body.tmp();
        self.body.line(format!("{null} = icmp eq ptr {base}, null"));
        let fail = self.body.label("null_pointer");
        let ok = self.body.label("address_ok");
        self.body
            .line(format!("br i1 {null}, label %{fail}, label %{ok}"));
        self.body.start(&fail);
        let code = self.cg.c_string("NullPointerError");
        let message = self.cg.c_string(&format!("cannot {action} null_pointer"));
        let location = self.cg.location(span);
        let status = self.body.tmp();
        self.body.line(format!(
            "{status} = call i32 @sayon_error_raise_text(ptr {err}, ptr {code}, ptr {message}, ptr {location})"
        ));
        self.body.line(format!("br label %b{}", error.0));
        self.body.start(&ok);
        let at = self.body.tmp();
        self.body
            .line(format!("{at} = getelementptr i8, ptr {base}, i64 {offset}"));
        at
    }

    fn widen(&mut self, ty: IntType, value: &str) -> String {
        if ty.bits() == 64 {
            return value.to_string();
        }
        let extended = self.body.tmp();
        let instruction = if ty.signed() { "sext" } else { "zext" };
        self.body.line(format!(
            "{extended} = {instruction} {} {value} to i64",
            int_llvm(ty)
        ));
        extended
    }

    #[allow(clippy::too_many_arguments)]
    fn int_arith(
        &mut self,
        op: ArithOp,
        ty: IntType,
        lhs: &Operand,
        rhs: &Operand,
        dest: Option<&Place>,
        span: Span,
        error: BlockId,
    ) -> String {
        let err = self.error_slot();
        let llvm = int_llvm(ty);
        let (_, a, _) = self.value(lhs);
        let (_, b, _) = self.value(rhs);
        let location = self.cg.location(span);
        let symbol = match op {
            ArithOp::Add => "+",
            ArithOp::Subtract => "-",
            ArithOp::Multiply => "*",
            ArithOp::Divide => "/",
            ArithOp::Remainder => "%",
        };
        let result = match op {
            ArithOp::Add | ArithOp::Subtract | ArithOp::Multiply => {
                let name = match op {
                    ArithOp::Add => "add",
                    ArithOp::Subtract => "sub",
                    _ => "mul",
                };
                let sign = if ty.signed() { "s" } else { "u" };
                let intrinsic = format!("llvm.{sign}{name}.with.overflow.{llvm}");
                self.cg.intrinsic(format!(
                    "declare {{ {llvm}, i1 }} @{intrinsic}({llvm}, {llvm})"
                ));
                let pair = self.body.tmp();
                self.body.line(format!(
                    "{pair} = call {{ {llvm}, i1 }} @{intrinsic}({llvm} {a}, {llvm} {b})"
                ));
                let result = self.body.tmp();
                self.body.line(format!(
                    "{result} = extractvalue {{ {llvm}, i1 }} {pair}, 0"
                ));
                let overflow = self.body.tmp();
                self.body.line(format!(
                    "{overflow} = extractvalue {{ {llvm}, i1 }} {pair}, 1"
                ));
                self.overflow_branch(&overflow, symbol, ty, &a, &b, &location, error);
                result
            }
            ArithOp::Divide | ArithOp::Remainder => {
                let zero = self.body.tmp();
                self.body.line(format!("{zero} = icmp eq {llvm} {b}, 0"));
                let fail = self.body.label("divide_by_zero");
                let ok = self.body.label("divisor_ok");
                self.body
                    .line(format!("br i1 {zero}, label %{fail}, label %{ok}"));
                self.body.start(&fail);
                let code = self.cg.c_string("DivisionByZeroError");
                let message = self.cg.c_string("integer division by zero");
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_error_raise_text(ptr {err}, ptr {code}, ptr {message}, ptr {location})"
                ));
                self.body.line(format!("br label %b{}", error.0));
                self.body.start(&ok);
                if ty.signed() {
                    // MIN / -1 overflows; MIN % -1 is 0.
                    let minimum = match ty.bits() {
                        8 => i8::MIN as i64,
                        16 => i16::MIN as i64,
                        32 => i32::MIN as i64,
                        _ => i64::MIN,
                    };
                    let is_minimum = self.body.tmp();
                    self.body
                        .line(format!("{is_minimum} = icmp eq {llvm} {a}, {minimum}"));
                    let is_negative_one = self.body.tmp();
                    self.body
                        .line(format!("{is_negative_one} = icmp eq {llvm} {b}, -1"));
                    let both = self.body.tmp();
                    self.body
                        .line(format!("{both} = and i1 {is_minimum}, {is_negative_one}"));
                    if op == ArithOp::Divide {
                        self.overflow_branch(&both, symbol, ty, &a, &b, &location, error);
                        let result = self.body.tmp();
                        self.body.line(format!("{result} = sdiv {llvm} {a}, {b}"));
                        result
                    } else {
                        let safe_divisor = self.body.tmp();
                        self.body.line(format!(
                            "{safe_divisor} = select i1 {both}, {llvm} 1, {llvm} {b}"
                        ));
                        let result = self.body.tmp();
                        self.body
                            .line(format!("{result} = srem {llvm} {a}, {safe_divisor}"));
                        result
                    }
                } else {
                    let instruction = if op == ArithOp::Divide {
                        "udiv"
                    } else {
                        "urem"
                    };
                    let result = self.body.tmp();
                    self.body
                        .line(format!("{result} = {instruction} {llvm} {a}, {b}"));
                    result
                }
            }
        };
        if let Some(place) = dest {
            let (pointer, _) = self.place_ptr(place);
            self.body
                .line(format!("store {llvm} {result}, ptr {pointer}"));
        }
        "0".to_string()
    }

    #[allow(clippy::too_many_arguments)]
    fn overflow_branch(
        &mut self,
        overflow: &str,
        symbol: &str,
        ty: IntType,
        a: &str,
        b: &str,
        location: &str,
        error: BlockId,
    ) {
        let err = self.error_slot();
        let fail = self.body.label("overflow");
        let ok = self.body.label("no_overflow");
        self.body
            .line(format!("br i1 {overflow}, label %{fail}, label %{ok}"));
        self.body.start(&fail);
        let wide_a = self.widen(ty, a);
        let wide_b = self.widen(ty, b);
        let symbol = self.cg.c_string(symbol);
        let status = self.body.tmp();
        self.body.line(format!(
            "{status} = call i32 @sayon_error_overflow(ptr {err}, ptr {symbol}, i64 {wide_a}, i64 {wide_b}, i32 {}, ptr {location})",
            int_code(ty)
        ));
        self.body.line(format!("br label %b{}", error.0));
        self.body.start(&ok);
    }

    fn convert(
        &mut self,
        from: &Type,
        to: &Type,
        value: &Operand,
        dest: Option<&Place>,
        span: Span,
    ) -> String {
        let err = self.error_slot();
        let (llvm, v, _) = self.value(value);
        let location = self.cg.location(span);
        let target = dest.map(|place| self.place_ptr(place).0);
        let store = |this: &mut Self, llvm: &str, value: &str| {
            if let Some(target) = &target {
                this.body
                    .line(format!("store {llvm} {value}, ptr {target}"));
            }
        };
        match (from, to) {
            (a, b) if a == b => {
                store(self, &llvm, &v);
                "0".to_string()
            }
            (Type::Int(source), Type::Int(destination)) => {
                let wide = self.widen(*source, &v);
                let out = self.body.alloca("i64");
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_int_convert(ptr {err}, i64 {wide}, i32 {}, i32 {}, ptr {location}, ptr {out})",
                    int_code(*source),
                    int_code(*destination)
                ));
                let loaded = self.body.tmp();
                self.body.line(format!("{loaded} = load i64, ptr {out}"));
                let result = if destination.bits() == 64 {
                    loaded
                } else {
                    let narrowed = self.body.tmp();
                    self.body.line(format!(
                        "{narrowed} = trunc i64 {loaded} to {}",
                        int_llvm(*destination)
                    ));
                    narrowed
                };
                store(self, int_llvm(*destination), &result);
                status
            }
            (Type::Int(source), Type::Float(float)) => {
                let float_llvm = if *float == FloatType::F64 {
                    "double"
                } else {
                    "float"
                };
                let instruction = if source.signed() { "sitofp" } else { "uitofp" };
                let result = self.body.tmp();
                self.body.line(format!(
                    "{result} = {instruction} {llvm} {v} to {float_llvm}"
                ));
                store(self, float_llvm, &result);
                "0".to_string()
            }
            (Type::Float(float), Type::Int(destination)) => {
                let wide = if *float == FloatType::F32 {
                    let extended = self.body.tmp();
                    self.body
                        .line(format!("{extended} = fpext float {v} to double"));
                    extended
                } else {
                    v.clone()
                };
                let out = self.body.alloca("i64");
                let status = self.body.tmp();
                self.body.line(format!(
                    "{status} = call i32 @sayon_float_to_int(ptr {err}, double {wide}, i32 {}, ptr {location}, ptr {out})",
                    int_code(*destination)
                ));
                let loaded = self.body.tmp();
                self.body.line(format!("{loaded} = load i64, ptr {out}"));
                let result = if destination.bits() == 64 {
                    loaded
                } else {
                    let narrowed = self.body.tmp();
                    self.body.line(format!(
                        "{narrowed} = trunc i64 {loaded} to {}",
                        int_llvm(*destination)
                    ));
                    narrowed
                };
                store(self, int_llvm(*destination), &result);
                status
            }
            (Type::Float(FloatType::F64), Type::Float(FloatType::F32)) => {
                let out = self.body.alloca("float");
                let status = self.body.tmp();
                self.body.line(format!("{status} = call i32 @sayon_f64_to_f32(ptr {err}, double {v}, ptr {location}, ptr {out})"));
                let loaded = self.body.tmp();
                self.body.line(format!("{loaded} = load float, ptr {out}"));
                store(self, "float", &loaded);
                status
            }
            (Type::Float(FloatType::F32), Type::Float(FloatType::F64)) => {
                let result = self.body.tmp();
                self.body
                    .line(format!("{result} = fpext float {v} to double"));
                store(self, "double", &result);
                "0".to_string()
            }
            (from, to) => panic!("unsupported conversion from {from} to {to}"),
        }
    }
}

pub(crate) const MEMCPY: &str = "declare void @llvm.memcpy.p0.p0.i64(ptr, ptr, i64, i1)";

const RUNTIME_DECLARATIONS: &str = "\
declare ptr @sayon_alloc(i64)
declare void @sayon_free(ptr, i64)
declare void @sayon_str_drop(ptr)
declare void @sayon_str_clone(ptr, ptr)
declare void @sayon_str_concat(ptr, ptr, ptr)
declare zeroext i1 @sayon_str_eq(ptr, ptr)
declare i32 @sayon_str_cmp(ptr, ptr)
declare i64 @sayon_str_chars(ptr)
declare i32 @sayon_str_char_at(ptr, ptr, i64, ptr, ptr)
declare void @sayon_str_next_char(ptr, ptr, ptr)
declare ptr @sayon_builder_new()
declare void @sayon_builder_bytes(ptr, ptr, i64)
declare void @sayon_builder_str(ptr, ptr, i1 zeroext)
declare void @sayon_builder_int(ptr, i64, i32)
declare void @sayon_builder_f64(ptr, double)
declare void @sayon_builder_f32(ptr, float)
declare void @sayon_builder_bool(ptr, i1 zeroext)
declare void @sayon_builder_error(ptr, ptr, i1 zeroext)
declare void @sayon_builder_range(ptr, ptr)
declare void @sayon_builder_finish(ptr, ptr)
declare i32 @sayon_print(ptr, ptr, ptr)
declare void @sayon_array_reserve(ptr, i64, i64)
declare void @sayon_array_free(ptr, i64)
declare i32 @sayon_check_index(ptr, i64, i64, ptr)
declare void @sayon_dict_new(ptr, ptr, i64)
declare zeroext i1 @sayon_dict_remove(ptr, ptr, ptr)
declare i32 @sayon_array_remove(ptr, ptr, i64, i64, ptr, ptr)
declare i32 @sayon_array_swap(ptr, ptr, i64, i64, i64, ptr)
declare i32 @sayon_check_view(ptr, i64, i64, i64, ptr)
declare i64 @sayon_str_hash(ptr)
declare i64 @sayon_dict_len(ptr)
declare i64 @sayon_dict_find(ptr, ptr)
declare i32 @sayon_dict_find_checked(ptr, ptr, ptr, ptr, ptr)
declare ptr @sayon_dict_key_ptr(ptr, i64)
declare ptr @sayon_dict_value_ptr(ptr, i64)
declare void @sayon_dict_set(ptr, ptr, ptr, ptr)
declare void @sayon_dict_drop(ptr, ptr)
declare void @sayon_dict_clone(ptr, ptr, ptr)
declare zeroext i1 @sayon_dict_eq(ptr, ptr, ptr)
declare i32 @sayon_error_raise(ptr, ptr, ptr, ptr)
declare i32 @sayon_error_raise_text(ptr, ptr, ptr, ptr)
declare i32 @sayon_error_overflow(ptr, ptr, i64, i64, i32, ptr)
declare i32 @sayon_error_negate(ptr, i64, i32, ptr)
declare void @sayon_error_drop(ptr)
declare void @sayon_stream_drop(ptr)
declare ptr @sayon_shared_new(i64)
declare void @sayon_shared_release(ptr, ptr)
declare void @sayon_shared_clone(ptr, ptr)
declare zeroext i1 @sayon_shared_upgrade(ptr)
declare void @sayon_weak_retain(ptr)
declare void @sayon_weak_release(ptr)
declare void @sayon_weak_clone(ptr, ptr)
declare i32 @sayon_check_split(ptr, i64, i64, ptr)
declare void @sayon_c_string_drop(ptr)
declare void @sayon_builder_c_string(ptr, ptr, i1)
declare void @sayon_builder_pointer(ptr, i64)
declare void @sayon_task_spawn(ptr, i64, ptr, ptr)
declare i32 @sayon_task_join(ptr, ptr, ptr, ptr)
declare void @sayon_task_detach(ptr)
declare void @sayon_task_drop(ptr)
declare i32 @sayon_task_yield(ptr, ptr)
declare i32 @sayon_task_sleep(ptr, i64, ptr)
declare i32 @sayon_tasks_finish(i1)
declare i32 @sayon_channel_new(ptr, i64, i64, ptr, ptr, ptr)
declare void @sayon_channel_clone(ptr, ptr)
declare void @sayon_channel_release(ptr)
declare i32 @sayon_channel_send(ptr, ptr, ptr, i1, ptr)
declare i32 @sayon_channel_receive(ptr, ptr, ptr, ptr)
declare void @sayon_channel_close(ptr)
declare i32 @sayon_channel_select(ptr, ptr, ptr, ptr)
declare void @sayon_stream_clone(ptr, ptr)
declare void @sayon_error_clone(ptr, ptr)
declare void @sayon_error_message(ptr, ptr)
declare void @sayon_error_code(ptr, ptr)
declare zeroext i1 @sayon_error_eq(ptr, ptr)
declare i32 @sayon_report_uncaught(ptr)
declare i32 @sayon_int_convert(ptr, i64, i32, i32, ptr, ptr)
declare i32 @sayon_float_to_int(ptr, double, i32, ptr, ptr)
declare i32 @sayon_f64_to_f32(ptr, double, ptr, ptr)
declare i32 @sayon_tonumber(ptr, ptr, ptr, ptr, ptr, ptr)
declare i32 @sayon_range_new(ptr, i64, i64, i64, ptr, ptr)
declare i64 @sayon_range_length(ptr)
declare i32 @sayon_assert_failed(ptr, ptr, ptr)
declare i32 @sayon_read_file(ptr, ptr, ptr, ptr)
declare i32 @sayon_write_file(ptr, ptr, ptr, ptr)
declare i32 @sayon_exit_status(i64)
declare void @sayon_runtime_finish()
declare void @sayon_runtime_init()
declare i32 @sayon_host_call(ptr, i32, ptr, ptr, ptr, ptr)
";
