//! Loads a program's modules from disk and runs the compiler phases.
//!
//! Import paths are resolved against the program root: the directory of the
//! entry file for a standalone script. `standard/...` paths resolve inside
//! the toolchain's standard library. Paths cannot leave their root, must
//! name an existing `.sayon` file with exactly the same spelling (also on
//! case-insensitive file systems), and imports cannot form a cycle.

use sayon_check::ModuleSource;
use sayon_syntax::ast::{self, ExprKind, StmtKind};
use sayon_syntax::{Diagnostic, Diagnostics, FileId, SourceMap, Span};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

pub struct Options {
    /// The standard library directory, if one was found.
    pub stdlib: Option<PathBuf>,
}

pub struct Output {
    pub sources: SourceMap,
    pub diagnostics: Diagnostics,
    /// The lowered program, when there were no errors.
    pub module: Option<sayon_ir::Module>,
}

/// The entry file could not be read at all.
pub struct EntryError(pub String);

struct Loaded {
    file: FileId,
    ast: ast::Module,
    name: String,
    root: PathBuf,
    standard: bool,
    imports: Vec<(String, Span, usize)>,
}

/// Loads, checks, and lowers the program whose entry file is `entry`.
pub fn compile(entry: &Path, options: &Options) -> Result<Output, EntryError> {
    let mut sources = SourceMap::new();
    let mut diagnostics = Diagnostics::new();
    let text = read_entry(entry)?;
    let entry_name = entry.display().to_string();
    let root = entry
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut loader = Loader {
        sources: &mut sources,
        diagnostics: &mut diagnostics,
        modules: Vec::new(),
        by_path: HashMap::new(),
        stdlib: options.stdlib.clone(),
    };
    let entry_file = match decode(entry, &entry_name, text, loader.sources) {
        Ok(file) => file,
        Err(diagnostic) => {
            diagnostics.push(diagnostic);
            return Ok(Output {
                sources,
                diagnostics,
                module: None,
            });
        }
    };
    loader.add(entry_file, entry_name, root, false, canonical(entry));
    loader.load_imports();
    let cycle_free = loader.check_cycles();
    let modules = std::mem::take(&mut loader.modules);

    if diagnostics.has_errors() || !cycle_free {
        diagnostics.sort();
        return Ok(Output {
            sources,
            diagnostics,
            module: None,
        });
    }
    let inputs: Vec<ModuleSource> = modules
        .iter()
        .map(|module| ModuleSource {
            ast: &module.ast,
            name: module.name.clone(),
            imports: module.imports.clone(),
            standard: module.standard,
        })
        .collect();
    let span = Span::new(modules[0].file, 0, 0);
    let checked = sayon_check::check_program(&inputs, 0, span);
    diagnostics.extend(checked.diagnostics);
    let module = match checked.program {
        Some(program) if !diagnostics.has_errors() => {
            let lowered = sayon_ir::lower(&program);
            diagnostics.extend(lowered.diagnostics);
            (!diagnostics.has_errors()).then_some(lowered.module)
        }
        _ => None,
    };
    diagnostics.sort();
    Ok(Output {
        sources,
        diagnostics,
        module,
    })
}

fn read_entry(path: &Path) -> Result<Vec<u8>, EntryError> {
    std::fs::read(path).map_err(|error| {
        let reason = match error.kind() {
            std::io::ErrorKind::NotFound => "the file does not exist".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            _ => error.to_string(),
        };
        EntryError(format!("cannot read `{}`: {reason}", path.display()))
    })
}

/// Adds a file to the source map, reporting invalid UTF-8.
fn decode(
    path: &Path,
    name: &str,
    bytes: Vec<u8>,
    sources: &mut SourceMap,
) -> Result<FileId, Diagnostic> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(sources.add(name, Some(path), text)),
        Err(error) => {
            let offset = error.utf8_error().valid_up_to();
            let text = String::from_utf8_lossy(error.as_bytes()).into_owned();
            let file = sources.add(name, Some(path), text);
            Err(
                Diagnostic::error("E_INVALID_UTF8", "source files must be valid UTF-8")
                    .primary(
                        Span::new(file, offset, offset + 1),
                        format!("invalid byte at offset {offset}"),
                    )
                    .help("save the file with UTF-8 encoding"),
            )
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct Loader<'s> {
    sources: &'s mut SourceMap,
    diagnostics: &'s mut Diagnostics,
    modules: Vec<Loaded>,
    by_path: HashMap<PathBuf, usize>,
    stdlib: Option<PathBuf>,
}

/// An `alias = import("path")` statement at the top of a module.
struct ImportStatement {
    alias: String,
    alias_span: Span,
    path: String,
    path_span: Span,
}

fn import_statements(module: &ast::Module) -> Vec<ImportStatement> {
    module
        .stmts
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            StmtKind::Assign { target, value } => match (&target.kind, &value.kind) {
                (ExprKind::Name(alias), ExprKind::Import { path, path_span }) => {
                    Some(ImportStatement {
                        alias: alias.name.clone(),
                        alias_span: alias.span,
                        path: path.clone(),
                        path_span: *path_span,
                    })
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

impl Loader<'_> {
    fn add(
        &mut self,
        file: FileId,
        name: String,
        root: PathBuf,
        standard: bool,
        key: PathBuf,
    ) -> usize {
        let text = self.sources.get(file).text.clone();
        let parsed = sayon_syntax::parser::parse_source(file, &text);
        self.diagnostics.extend(parsed.diagnostics);
        let index = self.modules.len();
        self.modules.push(Loaded {
            file,
            ast: parsed.module,
            name,
            root,
            standard,
            imports: Vec::new(),
        });
        self.by_path.insert(key, index);
        index
    }

    fn load_imports(&mut self) {
        let mut index = 0;
        while index < self.modules.len() {
            let statements = import_statements(&self.modules[index].ast);
            let mut seen: HashMap<String, Span> = HashMap::new();
            for statement in statements {
                if let Some(previous) = seen.get(&statement.alias) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E_DUPLICATE_DEFINITION",
                            format!("`{}` is imported twice", statement.alias),
                        )
                        .primary(statement.alias_span, "imported again here")
                        .secondary(*previous, "first imported here"),
                    );
                    continue;
                }
                seen.insert(statement.alias.clone(), statement.alias_span);
                if let Some(target) = self.resolve(index, &statement) {
                    self.modules[index].imports.push((
                        statement.alias.clone(),
                        statement.alias_span,
                        target,
                    ));
                }
            }
            index += 1;
        }
    }

    /// Resolves and loads one import, returning the imported module's index.
    fn resolve(&mut self, from: usize, statement: &ImportStatement) -> Option<usize> {
        let span = statement.path_span;
        let text = statement.path.as_str();
        let (root, relative, standard, name) = match text.strip_prefix("standard/") {
            Some(rest) => {
                let Some(stdlib) = self.stdlib.clone() else {
                    self.diagnostics.push(
                        Diagnostic::error("E_IMPORT_NOT_FOUND", "the standard library was not found")
                            .primary(span, "")
                            .help("set SAYON_STDLIB to the standard library directory of your Sayon installation"),
                    );
                    return None;
                };
                (stdlib, rest.to_string(), true, text.to_string())
            }
            None => {
                let module = &self.modules[from];
                (
                    module.root.clone(),
                    text.to_string(),
                    module.standard,
                    text.to_string(),
                )
            }
        };
        if !self.valid_relative_path(&relative, span) {
            return None;
        }
        let path = root.join(&relative);
        // Spelling is checked first so the result is the same on
        // case-sensitive and case-insensitive file systems.
        if let Some(actual) = spelling_mismatch(&root, &relative) {
            self.diagnostics.push(
                Diagnostic::error("E_IMPORT_CASE", format!("`{text}` does not match the file's spelling on disk"))
                    .primary(span, format!("the file is spelled `{actual}`"))
                    .help("import spelling is case-sensitive on every system so programs move between them unchanged"),
            );
            return None;
        }
        if !path.is_file() {
            self.diagnostics.push(
                Diagnostic::error("E_IMPORT_NOT_FOUND", format!("`{text}` was not found"))
                    .primary(span, "no such file in the program's root directory")
                    .help("import paths are relative to the program's root directory, the folder of the file you run"),
            );
            return None;
        }
        let key = canonical(&path);
        if let Some(index) = self.by_path.get(&key) {
            return Some(*index);
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.diagnostics.push(
                    Diagnostic::error("E_IMPORT_NOT_FOUND", format!("`{text}` could not be read"))
                        .primary(span, error.to_string()),
                );
                return None;
            }
        };
        match decode(&path, &name, bytes, self.sources) {
            Ok(file) => Some(self.add(file, name, root, standard, key)),
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                None
            }
        }
    }

    fn valid_relative_path(&mut self, path: &str, span: Span) -> bool {
        let problem = if !path.ends_with(".sayon") {
            Some("import paths name a `.sayon` file")
        } else if path.contains('\\') {
            Some("import paths use forward slashes on every system")
        } else if Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            Some("import paths stay inside the program's root: no `..`, `.`, or absolute paths")
        } else {
            None
        };
        if let Some(message) = problem {
            self.diagnostics
                .push(Diagnostic::error("E_IMPORT_PATH", message).primary(span, ""));
            return false;
        }
        true
    }

    /// Reports the first import cycle; returns false when there is one.
    fn check_cycles(&mut self) -> bool {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            New,
            Active,
            Done,
        }
        fn visit(
            loader: &mut Loader,
            node: usize,
            marks: &mut Vec<Mark>,
            stack: &mut Vec<(usize, Span)>,
        ) -> bool {
            marks[node] = Mark::Active;
            let imports = loader.modules[node].imports.clone();
            for (_, alias_span, target) in imports {
                stack.push((node, alias_span));
                match marks[target] {
                    Mark::Active => {
                        let start = stack
                            .iter()
                            .position(|(module, _)| *module == target)
                            .unwrap_or(0);
                        let cycle = &stack[start..];
                        let names: Vec<String> = cycle
                            .iter()
                            .map(|(module, _)| loader.modules[*module].name.clone())
                            .chain(std::iter::once(loader.modules[target].name.clone()))
                            .collect();
                        let mut diagnostic = Diagnostic::error(
                            "E_IMPORT_CYCLE",
                            format!(
                                "modules import each other in a cycle: {}",
                                names.join(" -> ")
                            ),
                        );
                        for (index, (_, span)) in cycle.iter().enumerate() {
                            diagnostic = if index + 1 == cycle.len() {
                                diagnostic.primary(*span, "this import completes the cycle")
                            } else {
                                diagnostic.secondary(*span, "imported here")
                            };
                        }
                        loader.diagnostics.push(diagnostic.help(
                            "move what both modules need into a third module that neither imports",
                        ));
                        return false;
                    }
                    Mark::New => {
                        if !visit(loader, target, marks, stack) {
                            return false;
                        }
                    }
                    Mark::Done => {}
                }
                stack.pop();
            }
            marks[node] = Mark::Done;
            true
        }
        let mut marks = vec![Mark::New; self.modules.len()];
        let mut stack = Vec::new();
        for node in 0..self.modules.len() {
            if marks[node] == Mark::New && !visit(self, node, &mut marks, &mut stack) {
                return false;
            }
        }
        true
    }
}

/// When a path exists but some component's spelling differs only in case,
/// returns the spelling on disk.
fn spelling_mismatch(root: &Path, relative: &str) -> Option<String> {
    let mut directory = root.to_path_buf();
    let mut actual = Vec::new();
    let mut mismatch = false;
    for component in relative.split('/') {
        let entries = std::fs::read_dir(&directory).ok()?;
        let found = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .find(|name| name.eq_ignore_ascii_case(component))?;
        if found != component {
            mismatch = true;
        }
        directory.push(&found);
        actual.push(found);
    }
    mismatch.then(|| actual.join("/"))
}
