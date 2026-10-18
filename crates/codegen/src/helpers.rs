//! Per-type helper functions: destruction, deep copy, equality, and display.
//!
//! - `drop(ptr)` releases what a value owns and leaves the place zeroed.
//! - `clone(ptr src, ptr dst)` initializes `dst` with an independent copy.
//! - `eq(ptr, ptr) -> i1` compares contents.
//! - `display(ptr builder, ptr value, i1 nested)` appends the `tostring` text.
//! - `envdrop(ptr block)` destroys a closure environment and frees its block.
//! - `envclone(ptr block) -> ptr` returns a new block with a deep copy.
//! - `hash(ptr) -> i64` hashes a dictionary key, consistently with `eq`.

use crate::emit::{Body, Codegen, Helper, MEMCPY};
use crate::layout::{int_code, int_llvm, needs_drop};
use sayon_check::types::{FloatType, NamedType, Type, structure_fields, variant_fields};
use std::fmt::Write as _;

impl<'m> Codegen<'m> {
    pub(crate) fn emit_helper(&mut self, kind: Helper, ty: &Type, name: &str) {
        let mut body = Body::new();
        let (ret, params) = match kind {
            Helper::Drop => ("void", "ptr %p"),
            Helper::Clone => ("void", "ptr %src, ptr %dst"),
            Helper::Eq => ("zeroext i1", "ptr %a, ptr %b"),
            Helper::Display => ("void", "ptr %builder, ptr %value, i1 zeroext %nested"),
            Helper::EnvDrop => ("void", "ptr %p"),
            Helper::EnvClone => ("ptr", "ptr %p"),
            Helper::Hash => ("i64", "ptr %p"),
        };
        match kind {
            Helper::Drop => self.drop_body(&mut body, ty),
            Helper::Clone => self.clone_body(&mut body, ty),
            Helper::Eq => self.eq_body(&mut body, ty),
            Helper::Display => self.display_body(&mut body, ty),
            Helper::EnvDrop => self.env_drop_body(&mut body, ty),
            Helper::EnvClone => self.env_clone_body(&mut body, ty),
            Helper::Hash => self.hash_body(&mut body, ty),
        }
        let _ = write!(
            self.helper_code,
            "; {kind:?} for {ty}\ndefine internal {ret} {name}({params}) {{\nentry:\n{}{}}}\n\n",
            body.allocas, body.code
        );
    }

    fn union_switch(
        &mut self,
        body: &mut Body,
        pointer: &str,
        ty: &Type,
        members: &[(u32, Type)],
        mut case: impl FnMut(&mut Self, &mut Body, &Type, &str),
    ) -> String {
        let tag = body.tmp();
        body.line(format!("{tag} = load i32, ptr {pointer}"));
        let done = body.label("union_done");
        let labels: Vec<(u32, String)> = members
            .iter()
            .map(|(t, _)| (*t, body.label("member")))
            .collect();
        let cases: Vec<String> = labels
            .iter()
            .map(|(t, label)| format!("i32 {t}, label %{label}"))
            .collect();
        body.line(format!(
            "switch i32 {tag}, label %{done} [ {} ]",
            cases.join(" ")
        ));
        for ((_, label), (_, member)) in labels.iter().zip(members) {
            body.start(label);
            case(self, body, member, &done);
        }
        let _ = ty;
        body.start(&done);
        tag
    }

    /// Branches on an enum value's tag; `case` runs for each listed variant
    /// and must end its block.
    fn enum_switch(
        &mut self,
        body: &mut Body,
        pointer: &str,
        variants: &[u32],
        mut case: impl FnMut(&mut Self, &mut Body, u32, &str),
    ) {
        let tag = body.tmp();
        body.line(format!("{tag} = load i32, ptr {pointer}"));
        let done = body.label("enum_done");
        let labels: Vec<String> = variants.iter().map(|_| body.label("variant")).collect();
        let cases: Vec<String> = variants
            .iter()
            .zip(&labels)
            .map(|(variant, label)| format!("i32 {}, label %{label}", variant + 1))
            .collect();
        body.line(format!(
            "switch i32 {tag}, label %{done} [ {} ]",
            cases.join(" ")
        ));
        for (variant, label) in variants.iter().zip(&labels) {
            body.start(label);
            case(self, body, *variant, &done);
        }
        body.start(&done);
    }

    /// The fields of a structure, or of one variant of an enum: their types
    /// and byte offsets within the whole value.
    fn parts(&self, named: &NamedType, variant: Option<u32>) -> Vec<(Type, u64)> {
        match variant {
            None => structure_fields(&self.module.types, named)
                .into_iter()
                .enumerate()
                .map(|(i, ty)| (ty, self.layouts.field_offset(named, i as u32)))
                .collect(),
            Some(variant) => variant_fields(&self.module.types, named, variant)
                .into_iter()
                .enumerate()
                .map(|(i, ty)| {
                    let offset = self.layouts.variant_field_offset(named, variant, i as u32);
                    (ty, offset)
                })
                .collect(),
        }
    }

    fn variant_count(&self, named: &NamedType) -> u32 {
        self.module.types[named.id.0 as usize].variants().len() as u32
    }

    /// Drops the fields that own something.
    fn drop_parts(&mut self, body: &mut Body, pointer: &str, parts: &[(Type, u64)]) {
        for (ty, offset) in parts {
            if needs_drop(ty) {
                let drop = self.helper(Helper::Drop, ty);
                let address = offset_pointer(body, pointer, *offset);
                body.line(format!("call void {drop}(ptr {address})"));
            }
        }
    }

    /// Replaces the shallow copies of owning fields in `%dst` with clones.
    fn clone_parts(&mut self, body: &mut Body, parts: &[(Type, u64)]) {
        for (ty, offset) in parts {
            if needs_drop(ty) {
                let clone = self.helper(Helper::Clone, ty);
                let from = offset_pointer(body, "%src", *offset);
                let to = offset_pointer(body, "%dst", *offset);
                body.line(format!("call void {clone}(ptr {from}, ptr {to})"));
            }
        }
    }

    /// Compares fields in order, branching to `different` at the first
    /// unequal pair.
    fn eq_parts(&mut self, body: &mut Body, parts: &[(Type, u64)], different: &str) {
        for (ty, offset) in parts {
            let eq = self.helper(Helper::Eq, ty);
            let a = offset_pointer(body, "%a", *offset);
            let b = offset_pointer(body, "%b", *offset);
            let same = body.tmp();
            body.line(format!("{same} = call zeroext i1 {eq}(ptr {a}, ptr {b})"));
            let next = body.label("same");
            body.line(format!("br i1 {same}, label %{next}, label %{different}"));
            body.start(&next);
        }
    }

    /// Member types of a union with their program-wide tags.
    fn tagged_members(&self, ty: &Type) -> Vec<(u32, Type)> {
        ty.union_members()
            .iter()
            .filter_map(|member| {
                self.module
                    .union_members
                    .iter()
                    .position(|m| m == member)
                    .map(|index| (index as u32 + 1, member.clone()))
            })
            .collect()
    }

    /// Folds the hashes of the parts at the given offsets into `acc`.
    fn hash_parts(&mut self, body: &mut Body, acc: &str, parts: &[(Type, u64)]) {
        for (ty, offset) in parts {
            let hash = self.helper(Helper::Hash, ty);
            let address = offset_pointer(body, "%p", *offset);
            let part = body.tmp();
            body.line(format!("{part} = call i64 {hash}(ptr {address})"));
            let current = body.tmp();
            body.line(format!("{current} = load i64, ptr {acc}"));
            let combined = combine(body, &current, &part);
            body.line(format!("store i64 {combined}, ptr {acc}"));
        }
    }

    fn hash_body(&mut self, body: &mut Body, ty: &Type) {
        match ty {
            Type::Int(int) => {
                let value = body.tmp();
                body.line(format!("{value} = load {}, ptr %p", int_llvm(*int)));
                if int.bits() == 64 {
                    body.line(format!("ret i64 {value}"));
                } else {
                    let wide = body.tmp();
                    let instruction = if int.signed() { "sext" } else { "zext" };
                    body.line(format!(
                        "{wide} = {instruction} {} {value} to i64",
                        int_llvm(*int)
                    ));
                    body.line(format!("ret i64 {wide}"));
                }
            }
            Type::Boolean => {
                let value = body.tmp();
                body.line(format!("{value} = load i1, ptr %p"));
                let wide = body.tmp();
                body.line(format!("{wide} = zext i1 {value} to i64"));
                body.line(format!("ret i64 {wide}"));
            }
            Type::String => {
                let hash = body.tmp();
                body.line(format!("{hash} = call i64 @sayon_str_hash(ptr %p)"));
                body.line(format!("ret i64 {hash}"));
            }
            Type::Union(_) => {
                let acc = body.alloca("i64");
                body.line(format!("store i64 0, ptr {acc}"));
                let members = self.tagged_members(ty);
                let tag = self.union_switch(body, "%p", ty, &members, |cg, b, member, done| {
                    let hash = cg.helper(Helper::Hash, member);
                    let payload = offset_pointer(b, "%p", 8);
                    let part = b.tmp();
                    b.line(format!("{part} = call i64 {hash}(ptr {payload})"));
                    b.line(format!("store i64 {part}, ptr {acc}"));
                    b.line(format!("br label %{done}"));
                });
                let wide = body.tmp();
                body.line(format!("{wide} = zext i32 {tag} to i64"));
                let part = body.tmp();
                body.line(format!("{part} = load i64, ptr {acc}"));
                let combined = combine(body, &wide, &part);
                body.line(format!("ret i64 {combined}"));
            }
            Type::Named(named) if self.layouts.is_enum(named) => {
                let acc = body.alloca("i64");
                let tag = body.tmp();
                body.line(format!("{tag} = load i32, ptr %p"));
                let wide = body.tmp();
                body.line(format!("{wide} = zext i32 {tag} to i64"));
                body.line(format!("store i64 {wide}, ptr {acc}"));
                let variants: Vec<u32> = (0..self.variant_count(named))
                    .filter(|v| !self.parts(named, Some(*v)).is_empty())
                    .collect();
                self.enum_switch(body, "%p", &variants, |cg, b, variant, done| {
                    let parts = cg.parts(named, Some(variant));
                    cg.hash_parts(b, &acc, &parts);
                    b.line(format!("br label %{done}"));
                });
                let result = body.tmp();
                body.line(format!("{result} = load i64, ptr {acc}"));
                body.line(format!("ret i64 {result}"));
            }
            Type::Named(named) => {
                let acc = body.alloca("i64");
                body.line(format!("store i64 0, ptr {acc}"));
                let parts = self.parts(named, None);
                self.hash_parts(body, &acc, &parts);
                let result = body.tmp();
                body.line(format!("{result} = load i64, ptr {acc}"));
                body.line(format!("ret i64 {result}"));
            }
            // `nothing`, and types the checker never accepts as keys.
            _ => body.line("ret i64 0"),
        }
    }

    fn env_drop_body(&mut self, body: &mut Body, env: &Type) {
        if needs_drop(env) {
            let drop = self.helper(Helper::Drop, env);
            let record = offset_pointer(body, "%p", 16);
            body.line(format!("call void {drop}(ptr {record})"));
        }
        let size = 16 + self.layouts.size_of(env);
        body.line(format!("call void @sayon_free(ptr %p, i64 {size})"));
        body.line("ret void");
    }

    fn env_clone_body(&mut self, body: &mut Body, env: &Type) {
        let size = 16 + self.layouts.size_of(env);
        let block = body.tmp();
        body.line(format!("{block} = call ptr @sayon_alloc(i64 {size})"));
        self.intrinsic(MEMCPY.to_string());
        body.memcpy(&block, "%p", &size.to_string());
        if needs_drop(env) {
            let clone = self.helper(Helper::Clone, env);
            let from = offset_pointer(body, "%p", 16);
            let to = offset_pointer(body, &block, 16);
            body.line(format!("call void {clone}(ptr {from}, ptr {to})"));
        }
        body.line(format!("ret ptr {block}"));
    }

    fn drop_body(&mut self, body: &mut Body, ty: &Type) {
        match ty {
            Type::Function(_) => {
                let field = offset_pointer(body, "%p", 8);
                let block = body.tmp();
                body.line(format!("{block} = load ptr, ptr {field}"));
                let none = body.tmp();
                body.line(format!("{none} = icmp eq ptr {block}, null"));
                let release = body.label("release");
                let done = body.label("done");
                body.line(format!("br i1 {none}, label %{done}, label %{release}"));
                body.start(&release);
                let drop = body.tmp();
                body.line(format!("{drop} = load ptr, ptr {block}"));
                body.line(format!("call void {drop}(ptr {block})"));
                body.line(format!("br label %{done}"));
                body.start(&done);
                body.line("store %sayon.func zeroinitializer, ptr %p");
            }
            Type::String => body.line("call void @sayon_str_drop(ptr %p)"),
            Type::Error => body.line("call void @sayon_error_drop(ptr %p)"),
            Type::Stream => body.line("call void @sayon_stream_drop(ptr %p)"),
            Type::Dict(_, value) => {
                let drop = if needs_drop(value) {
                    self.helper(Helper::Drop, value)
                } else {
                    "null".to_string()
                };
                body.line(format!("call void @sayon_dict_drop(ptr %p, ptr {drop})"));
            }
            Type::Array(element) => {
                if needs_drop(element) {
                    let drop = self.helper(Helper::Drop, element);
                    let element_llvm = self.llvm(element);
                    let length = load_length(body, "%p");
                    let data = body.tmp();
                    body.line(format!("{data} = load ptr, ptr %p"));
                    body.counted_loop(&length, |b, index| {
                        let address = b.tmp();
                        b.line(format!("{address} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {index}"));
                        b.line(format!("call void {drop}(ptr {address})"));
                    });
                }
                body.line(format!(
                    "call void @sayon_array_free(ptr %p, i64 {})",
                    self.layouts.size_of(element)
                ));
            }
            Type::Union(_) => {
                let members: Vec<(u32, Type)> = self
                    .tagged_members(ty)
                    .into_iter()
                    .filter(|(_, m)| needs_drop(m))
                    .collect();
                self.union_switch(body, "%p", ty, &members, |cg, b, member, done| {
                    let payload = b.tmp();
                    b.line(format!(
                        "{payload} = getelementptr inbounds i8, ptr %p, i64 8"
                    ));
                    let drop = cg.helper(Helper::Drop, member);
                    b.line(format!("call void {drop}(ptr {payload})"));
                    b.line(format!("br label %{done}"));
                });
                let llvm = self.llvm(ty);
                body.line(format!("store {llvm} zeroinitializer, ptr %p"));
            }
            Type::Named(named) if self.layouts.is_enum(named) => {
                let variants: Vec<u32> = (0..self.variant_count(named))
                    .filter(|v| {
                        self.parts(named, Some(*v))
                            .iter()
                            .any(|(t, _)| needs_drop(t))
                    })
                    .collect();
                self.enum_switch(body, "%p", &variants, |cg, b, variant, done| {
                    let parts = cg.parts(named, Some(variant));
                    cg.drop_parts(b, "%p", &parts);
                    b.line(format!("br label %{done}"));
                });
                let llvm = self.llvm(ty);
                body.line(format!("store {llvm} zeroinitializer, ptr %p"));
            }
            Type::Named(named) => {
                let parts = self.parts(named, None);
                self.drop_parts(body, "%p", &parts);
            }
            _ => {}
        }
        body.line("ret void");
    }

    fn clone_body(&mut self, body: &mut Body, ty: &Type) {
        match ty {
            Type::Function(_) => {
                self.intrinsic(MEMCPY.to_string());
                body.memcpy("%dst", "%src", "16");
                let field = offset_pointer(body, "%src", 8);
                let block = body.tmp();
                body.line(format!("{block} = load ptr, ptr {field}"));
                let none = body.tmp();
                body.line(format!("{none} = icmp eq ptr {block}, null"));
                let copy = body.label("copy");
                let done = body.label("done");
                body.line(format!("br i1 {none}, label %{done}, label %{copy}"));
                body.start(&copy);
                let clone_field = offset_pointer(body, &block, 8);
                let clone = body.tmp();
                body.line(format!("{clone} = load ptr, ptr {clone_field}"));
                let copied = body.tmp();
                body.line(format!("{copied} = call ptr {clone}(ptr {block})"));
                let target = offset_pointer(body, "%dst", 8);
                body.line(format!("store ptr {copied}, ptr {target}"));
                body.line(format!("br label %{done}"));
                body.start(&done);
            }
            Type::String => body.line("call void @sayon_str_clone(ptr %src, ptr %dst)"),
            Type::Error => body.line("call void @sayon_error_clone(ptr %src, ptr %dst)"),
            Type::Stream => body.line("call void @sayon_stream_clone(ptr %src, ptr %dst)"),
            Type::Dict(_, value) => {
                let clone = if needs_drop(value) {
                    self.helper(Helper::Clone, value)
                } else {
                    "null".to_string()
                };
                let handle = body.tmp();
                body.line(format!("{handle} = load ptr, ptr %src"));
                body.line(format!(
                    "call void @sayon_dict_clone(ptr {handle}, ptr %dst, ptr {clone})"
                ));
            }
            Type::Array(element) => {
                let size = self.layouts.size_of(element);
                body.line("store %sayon.array zeroinitializer, ptr %dst");
                let length = load_length(body, "%src");
                body.line(format!(
                    "call void @sayon_array_reserve(ptr %dst, i64 {length}, i64 {size})"
                ));
                let source = body.tmp();
                body.line(format!("{source} = load ptr, ptr %src"));
                let target = body.tmp();
                body.line(format!("{target} = load ptr, ptr %dst"));
                if needs_drop(element) {
                    let clone = self.helper(Helper::Clone, element);
                    let element_llvm = self.llvm(element);
                    body.counted_loop(&length, |b, index| {
                        let from = b.tmp();
                        b.line(format!("{from} = getelementptr inbounds {element_llvm}, ptr {source}, i64 {index}"));
                        let to = b.tmp();
                        b.line(format!("{to} = getelementptr inbounds {element_llvm}, ptr {target}, i64 {index}"));
                        b.line(format!("call void {clone}(ptr {from}, ptr {to})"));
                    });
                } else {
                    let bytes = body.tmp();
                    body.line(format!("{bytes} = mul i64 {length}, {size}"));
                    self.intrinsic(MEMCPY.to_string());
                    body.memcpy(&target, &source, &bytes);
                }
                let field = body.tmp();
                body.line(format!(
                    "{field} = getelementptr inbounds %sayon.array, ptr %dst, i32 0, i32 1"
                ));
                body.line(format!("store i64 {length}, ptr {field}"));
            }
            Type::Union(_) => {
                self.intrinsic(MEMCPY.to_string());
                body.memcpy("%dst", "%src", &self.layouts.size_of(ty).to_string());
                let members: Vec<(u32, Type)> = self
                    .tagged_members(ty)
                    .into_iter()
                    .filter(|(_, m)| needs_drop(m))
                    .collect();
                self.union_switch(body, "%src", ty, &members, |cg, b, member, done| {
                    let from = b.tmp();
                    b.line(format!(
                        "{from} = getelementptr inbounds i8, ptr %src, i64 8"
                    ));
                    let to = b.tmp();
                    b.line(format!("{to} = getelementptr inbounds i8, ptr %dst, i64 8"));
                    let clone = cg.helper(Helper::Clone, member);
                    b.line(format!("call void {clone}(ptr {from}, ptr {to})"));
                    b.line(format!("br label %{done}"));
                });
            }
            Type::Named(named) if needs_drop(ty) => {
                self.intrinsic(MEMCPY.to_string());
                body.memcpy("%dst", "%src", &self.layouts.size_of(ty).to_string());
                if self.layouts.is_enum(named) {
                    let variants: Vec<u32> = (0..self.variant_count(named))
                        .filter(|v| {
                            self.parts(named, Some(*v))
                                .iter()
                                .any(|(t, _)| needs_drop(t))
                        })
                        .collect();
                    self.enum_switch(body, "%src", &variants, |cg, b, variant, done| {
                        let parts = cg.parts(named, Some(variant));
                        cg.clone_parts(b, &parts);
                        b.line(format!("br label %{done}"));
                    });
                } else {
                    let parts = self.parts(named, None);
                    self.clone_parts(body, &parts);
                }
            }
            _ => {
                let llvm = self.llvm(ty);
                let value = body.tmp();
                body.line(format!("{value} = load {llvm}, ptr %src"));
                body.line(format!("store {llvm} {value}, ptr %dst"));
            }
        }
        body.line("ret void");
    }

    fn eq_body(&mut self, body: &mut Body, ty: &Type) {
        let scalar = |body: &mut Body, llvm: &str, instruction: &str| {
            let a = body.tmp();
            body.line(format!("{a} = load {llvm}, ptr %a"));
            let b = body.tmp();
            body.line(format!("{b} = load {llvm}, ptr %b"));
            let result = body.tmp();
            body.line(format!("{result} = {instruction} {llvm} {a}, {b}"));
            body.line(format!("ret i1 {result}"));
        };
        match ty {
            Type::Int(int) => scalar(body, int_llvm(*int), "icmp eq"),
            Type::Float(FloatType::F64) => scalar(body, "double", "fcmp oeq"),
            Type::Float(FloatType::F32) => scalar(body, "float", "fcmp oeq"),
            Type::Boolean => scalar(body, "i1", "icmp eq"),
            Type::Nothing | Type::Unknown => body.line("ret i1 true"),
            // Function values, views, and streams are never compared.
            Type::Function(_) | Type::View(..) | Type::Stream => body.line("ret i1 false"),
            Type::String => {
                let result = body.tmp();
                body.line(format!(
                    "{result} = call zeroext i1 @sayon_str_eq(ptr %a, ptr %b)"
                ));
                body.line(format!("ret i1 {result}"));
            }
            Type::Error => {
                let result = body.tmp();
                body.line(format!(
                    "{result} = call zeroext i1 @sayon_error_eq(ptr %a, ptr %b)"
                ));
                body.line(format!("ret i1 {result}"));
            }
            Type::Range => {
                let mut all = "true".to_string();
                for field in 0..3 {
                    let pa = body.tmp();
                    body.line(format!(
                        "{pa} = getelementptr inbounds %sayon.range, ptr %a, i32 0, i32 {field}"
                    ));
                    let pb = body.tmp();
                    body.line(format!(
                        "{pb} = getelementptr inbounds %sayon.range, ptr %b, i32 0, i32 {field}"
                    ));
                    let va = body.tmp();
                    body.line(format!("{va} = load i64, ptr {pa}"));
                    let vb = body.tmp();
                    body.line(format!("{vb} = load i64, ptr {pb}"));
                    let same = body.tmp();
                    body.line(format!("{same} = icmp eq i64 {va}, {vb}"));
                    let combined = body.tmp();
                    body.line(format!("{combined} = and i1 {all}, {same}"));
                    all = combined;
                }
                body.line(format!("ret i1 {all}"));
            }
            Type::Dict(_, value) => {
                let eq = self.helper(Helper::Eq, value);
                let ha = body.tmp();
                body.line(format!("{ha} = load ptr, ptr %a"));
                let hb = body.tmp();
                body.line(format!("{hb} = load ptr, ptr %b"));
                let result = body.tmp();
                body.line(format!(
                    "{result} = call zeroext i1 @sayon_dict_eq(ptr {ha}, ptr {hb}, ptr {eq})"
                ));
                body.line(format!("ret i1 {result}"));
            }
            Type::Array(element) => {
                let eq = self.helper(Helper::Eq, element);
                let element_llvm = self.llvm(element);
                let la = load_length(body, "%a");
                let lb = load_length(body, "%b");
                let same_length = body.tmp();
                body.line(format!("{same_length} = icmp eq i64 {la}, {lb}"));
                let compare = body.label("compare");
                let different = body.label("different");
                body.line(format!(
                    "br i1 {same_length}, label %{compare}, label %{different}"
                ));
                body.start(&compare);
                let da = body.tmp();
                body.line(format!("{da} = load ptr, ptr %a"));
                let db = body.tmp();
                body.line(format!("{db} = load ptr, ptr %b"));
                let different_label = different.clone();
                body.counted_loop(&la, |b, index| {
                    let ea = b.tmp();
                    b.line(format!(
                        "{ea} = getelementptr inbounds {element_llvm}, ptr {da}, i64 {index}"
                    ));
                    let eb = b.tmp();
                    b.line(format!(
                        "{eb} = getelementptr inbounds {element_llvm}, ptr {db}, i64 {index}"
                    ));
                    let same = b.tmp();
                    b.line(format!("{same} = call zeroext i1 {eq}(ptr {ea}, ptr {eb})"));
                    let next = b.label("same");
                    b.line(format!(
                        "br i1 {same}, label %{next}, label %{different_label}"
                    ));
                    b.start(&next);
                });
                body.line("ret i1 true");
                body.start(&different);
                body.line("ret i1 false");
            }
            Type::Union(_) => {
                let ta = body.tmp();
                body.line(format!("{ta} = load i32, ptr %a"));
                let tb = body.tmp();
                body.line(format!("{tb} = load i32, ptr %b"));
                let same_tag = body.tmp();
                body.line(format!("{same_tag} = icmp eq i32 {ta}, {tb}"));
                let compare = body.label("compare");
                let different = body.label("different");
                body.line(format!(
                    "br i1 {same_tag}, label %{compare}, label %{different}"
                ));
                body.start(&compare);
                let members = self.tagged_members(ty);
                self.union_switch(body, "%a", ty, &members, |cg, b, member, _| {
                    let pa = b.tmp();
                    b.line(format!("{pa} = getelementptr inbounds i8, ptr %a, i64 8"));
                    let pb = b.tmp();
                    b.line(format!("{pb} = getelementptr inbounds i8, ptr %b, i64 8"));
                    let eq = cg.helper(Helper::Eq, member);
                    let result = b.tmp();
                    b.line(format!(
                        "{result} = call zeroext i1 {eq}(ptr {pa}, ptr {pb})"
                    ));
                    b.line(format!("ret i1 {result}"));
                });
                body.line("ret i1 true");
                body.start(&different);
                body.line("ret i1 false");
            }
            Type::Named(named) if self.layouts.is_enum(named) => {
                let ta = body.tmp();
                body.line(format!("{ta} = load i32, ptr %a"));
                let tb = body.tmp();
                body.line(format!("{tb} = load i32, ptr %b"));
                let same_tag = body.tmp();
                body.line(format!("{same_tag} = icmp eq i32 {ta}, {tb}"));
                let compare = body.label("compare");
                let different = body.label("different");
                body.line(format!(
                    "br i1 {same_tag}, label %{compare}, label %{different}"
                ));
                body.start(&compare);
                let variants: Vec<u32> = (0..self.variant_count(named))
                    .filter(|v| !self.parts(named, Some(*v)).is_empty())
                    .collect();
                let different_label = different.clone();
                self.enum_switch(body, "%a", &variants, |cg, b, variant, _| {
                    let parts = cg.parts(named, Some(variant));
                    cg.eq_parts(b, &parts, &different_label);
                    b.line("ret i1 true");
                });
                body.line("ret i1 true");
                body.start(&different);
                body.line("ret i1 false");
            }
            Type::Named(named) => {
                let different = body.label("different");
                let parts = self.parts(named, None);
                self.eq_parts(body, &parts, &different);
                body.line("ret i1 true");
                body.start(&different);
                body.line("ret i1 false");
            }
            Type::Param(_) | Type::Var(_) => body.line("ret i1 false"),
        }
    }

    fn display_body(&mut self, body: &mut Body, ty: &Type) {
        match ty {
            Type::Int(int) => {
                let value = body.tmp();
                body.line(format!("{value} = load {}, ptr %value", int_llvm(*int)));
                let wide = if int.bits() == 64 {
                    value
                } else {
                    let extended = body.tmp();
                    let instruction = if int.signed() { "sext" } else { "zext" };
                    body.line(format!(
                        "{extended} = {instruction} {} {value} to i64",
                        int_llvm(*int)
                    ));
                    extended
                };
                body.line(format!(
                    "call void @sayon_builder_int(ptr %builder, i64 {wide}, i32 {})",
                    int_code(*int)
                ));
            }
            Type::Float(FloatType::F64) => {
                let value = body.tmp();
                body.line(format!("{value} = load double, ptr %value"));
                body.line(format!(
                    "call void @sayon_builder_f64(ptr %builder, double {value})"
                ));
            }
            Type::Float(FloatType::F32) => {
                let value = body.tmp();
                body.line(format!("{value} = load float, ptr %value"));
                body.line(format!(
                    "call void @sayon_builder_f32(ptr %builder, float {value})"
                ));
            }
            Type::Boolean => {
                let value = body.tmp();
                body.line(format!("{value} = load i1, ptr %value"));
                body.line(format!(
                    "call void @sayon_builder_bool(ptr %builder, i1 {value})"
                ));
            }
            Type::Nothing | Type::Unknown => self.text(body, "nothing"),
            Type::Function(_) => self.text(body, "function"),
            Type::Stream => self.text(body, "stream"),
            Type::String => {
                body.line("call void @sayon_builder_str(ptr %builder, ptr %value, i1 %nested)")
            }
            Type::Error => {
                body.line("call void @sayon_builder_error(ptr %builder, ptr %value, i1 %nested)")
            }
            Type::Range => body.line("call void @sayon_builder_range(ptr %builder, ptr %value)"),
            // A view has an array's first two fields, so it displays like one.
            Type::Array(element) | Type::View(element, _) => {
                let display = self.helper(Helper::Display, element);
                let element_llvm = self.llvm(element);
                let open = self.bytes(b"[");
                let separator = self.bytes(b", ");
                let close = self.bytes(b"]");
                body.line(format!(
                    "call void @sayon_builder_bytes(ptr %builder, ptr {open}, i64 1)"
                ));
                let length = load_length(body, "%value");
                let data = body.tmp();
                body.line(format!("{data} = load ptr, ptr %value"));
                body.counted_loop(&length, |b, index| {
                    separator_unless_first(b, index, &separator);
                    let address = b.tmp();
                    b.line(format!(
                        "{address} = getelementptr inbounds {element_llvm}, ptr {data}, i64 {index}"
                    ));
                    b.line(format!(
                        "call void {display}(ptr %builder, ptr {address}, i1 true)"
                    ));
                });
                body.line(format!(
                    "call void @sayon_builder_bytes(ptr %builder, ptr {close}, i64 1)"
                ));
            }
            Type::Dict(key_ty, value) => {
                let display = self.helper(Helper::Display, value);
                let key_display = self.helper(Helper::Display, key_ty);
                let open = self.bytes(b"[");
                let separator = self.bytes(b", ");
                let equals = self.bytes(b" = ");
                let close = self.bytes(b"]");
                body.line(format!(
                    "call void @sayon_builder_bytes(ptr %builder, ptr {open}, i64 1)"
                ));
                let handle = body.tmp();
                body.line(format!("{handle} = load ptr, ptr %value"));
                let length = body.tmp();
                body.line(format!("{length} = call i64 @sayon_dict_len(ptr {handle})"));
                body.counted_loop(&length, |b, index| {
                    separator_unless_first(b, index, &separator);
                    let key = b.tmp();
                    b.line(format!(
                        "{key} = call ptr @sayon_dict_key_ptr(ptr {handle}, i64 {index})"
                    ));
                    b.line(format!(
                        "call void {key_display}(ptr %builder, ptr {key}, i1 true)"
                    ));
                    b.line(format!(
                        "call void @sayon_builder_bytes(ptr %builder, ptr {equals}, i64 3)"
                    ));
                    let entry = b.tmp();
                    b.line(format!(
                        "{entry} = call ptr @sayon_dict_value_ptr(ptr {handle}, i64 {index})"
                    ));
                    b.line(format!(
                        "call void {display}(ptr %builder, ptr {entry}, i1 true)"
                    ));
                });
                body.line(format!(
                    "call void @sayon_builder_bytes(ptr %builder, ptr {close}, i64 1)"
                ));
            }
            Type::Union(_) => {
                let members = self.tagged_members(ty);
                self.union_switch(body, "%value", ty, &members, |cg, b, member, done| {
                    let payload = b.tmp();
                    b.line(format!(
                        "{payload} = getelementptr inbounds i8, ptr %value, i64 8"
                    ));
                    let display = cg.helper(Helper::Display, member);
                    b.line(format!(
                        "call void {display}(ptr %builder, ptr {payload}, i1 %nested)"
                    ));
                    b.line(format!("br label %{done}"));
                });
            }
            Type::Named(named) if self.layouts.is_enum(named) => {
                let variants: Vec<u32> = (0..self.variant_count(named)).collect();
                self.enum_switch(body, "%value", &variants, |cg, b, variant, done| {
                    let def = &cg.module.types[named.id.0 as usize];
                    let name = def.variants()[variant as usize].name.clone();
                    cg.text(b, &name);
                    let parts = cg.parts(named, Some(variant));
                    if !parts.is_empty() {
                        cg.text(b, "(");
                        for (index, (ty, offset)) in parts.iter().enumerate() {
                            if index > 0 {
                                cg.text(b, ", ");
                            }
                            cg.display_part(b, ty, *offset);
                        }
                        cg.text(b, ")");
                    }
                    b.line(format!("br label %{done}"));
                });
            }
            Type::Named(named) => {
                let def = &self.module.types[named.id.0 as usize];
                let names: Vec<String> = def.fields().iter().map(|f| f.name.clone()).collect();
                self.text(body, &format!("{}(", def.name));
                let parts = self.parts(named, None);
                for (index, ((ty, offset), name)) in parts.iter().zip(&names).enumerate() {
                    let label = if index > 0 {
                        format!(", {name} = ")
                    } else {
                        format!("{name} = ")
                    };
                    self.text(body, &label);
                    self.display_part(body, ty, *offset);
                }
                self.text(body, ")");
            }
            Type::Param(_) | Type::Var(_) => {}
        }
        body.line("ret void");
    }

    /// Displays one field of `%value`, as a nested value.
    fn display_part(&mut self, body: &mut Body, ty: &Type, offset: u64) {
        let display = self.helper(Helper::Display, ty);
        let address = offset_pointer(body, "%value", offset);
        body.line(format!(
            "call void {display}(ptr %builder, ptr {address}, i1 true)"
        ));
    }

    fn text(&mut self, body: &mut Body, text: &str) {
        let constant = self.bytes(text.as_bytes());
        body.line(format!(
            "call void @sayon_builder_bytes(ptr %builder, ptr {constant}, i64 {})",
            text.len()
        ));
    }
}

/// Mixes a part's hash into a running hash.
fn combine(body: &mut Body, running: &str, part: &str) -> String {
    let mixed = body.tmp();
    body.line(format!("{mixed} = xor i64 {running}, {part}"));
    let result = body.tmp();
    body.line(format!("{result} = mul i64 {mixed}, 1099511628211"));
    result
}

/// `pointer + offset` bytes.
fn offset_pointer(body: &mut Body, pointer: &str, offset: u64) -> String {
    if offset == 0 {
        return pointer.to_string();
    }
    let address = body.tmp();
    body.line(format!(
        "{address} = getelementptr inbounds i8, ptr {pointer}, i64 {offset}"
    ));
    address
}

fn load_length(body: &mut Body, array: &str) -> String {
    let field = body.tmp();
    body.line(format!(
        "{field} = getelementptr inbounds %sayon.array, ptr {array}, i32 0, i32 1"
    ));
    let length = body.tmp();
    body.line(format!("{length} = load i64, ptr {field}"));
    length
}

fn separator_unless_first(body: &mut Body, index: &str, separator: &str) {
    let first = body.tmp();
    body.line(format!("{first} = icmp eq i64 {index}, 0"));
    let write = body.label("separator");
    let next = body.label("item");
    body.line(format!("br i1 {first}, label %{next}, label %{write}"));
    body.start(&write);
    body.line(format!(
        "call void @sayon_builder_bytes(ptr %builder, ptr {separator}, i64 2)"
    ));
    body.line(format!("br label %{next}"));
    body.start(&next);
}
