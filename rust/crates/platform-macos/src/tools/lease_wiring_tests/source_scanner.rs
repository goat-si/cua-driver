//! Syntactic inventory of worker creation sites. This is a regression guard,
//! not proof that an arbitrary native effect runs under an admitted lease.

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ItemMod, Meta, UseTree};

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct Site {
    pub file: String,
    pub item: String,
    pub primitive: String,
    pub fingerprint: u64,
    pub occurrence: usize,
    pub line: usize,
}

fn cfg_value(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            use syn::parse::Parser;
            let args = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let values: Vec<_> = args.iter().map(cfg_value).collect();
            if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|v| *v == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if values.contains(&Some(true)) {
                Some(true)
            } else if values.iter().all(|v| *v == Some(false)) {
                Some(false)
            } else {
                None
            }
        }
        Meta::List(list) if list.path.is_ident("not") => syn::parse2::<Meta>(list.tokens.clone())
            .ok()
            .and_then(|m| cfg_value(&m))
            .map(|v| !v),
        _ => None,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<Meta>().ok().and_then(|m| cfg_value(&m)) == Some(false)
    })
}

fn fingerprint(tokens: TokenStream) -> u64 {
    // A deterministic token fingerprint, independent of whitespace and comments.
    tokens
        .to_string()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        })
}

fn imports(tree: &UseTree, prefix: &mut Vec<String>, out: &mut BTreeMap<String, BTreeSet<String>>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            imports(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let name = n.ident.to_string();
            let mut path = prefix.clone();
            path.push(name.clone());
            out.entry(name).or_default().insert(path.join("::"));
        }
        UseTree::Rename(n) => {
            let mut path = prefix.clone();
            path.push(n.ident.to_string());
            out.entry(n.rename.to_string())
                .or_default()
                .insert(path.join("::"));
        }
        UseTree::Group(g) => {
            for item in &g.items {
                imports(item, prefix, out);
            }
        }
        UseTree::Glob(_) => {
            out.entry("*".into()).or_default().insert(prefix.join("::"));
        }
    }
}

struct Scanner {
    file: String,
    item: Vec<String>,
    aliases: BTreeMap<String, BTreeSet<String>>,
    counts: BTreeMap<(String, String, u64), usize>,
    sites: Vec<Site>,
}

impl Scanner {
    fn wrapped_thread_builder(&self, call: &syn::ExprMethodCall) -> bool {
        fn builder(scanner: &Scanner, expr: &Expr) -> bool {
            match expr {
                Expr::MethodCall(method) => builder(scanner, &method.receiver),
                Expr::Call(call) => match &*call.func {
                    Expr::Path(path) => scanner
                        .resolve(&path.path)
                        .iter()
                        .any(|name| name == "std::thread::Builder::new"),
                    _ => false,
                },
                _ => false,
            }
        }
        if call.method != "spawn" || call.args.len() != 1 || !builder(self, &call.receiver) {
            return false;
        }
        match call.args.first() {
            Some(Expr::Call(wrap)) if wrap.args.len() == 1 => match &*wrap.func {
                Expr::Path(path) => self
                    .resolve(&path.path)
                    .iter()
                    .any(|name| name == "crate::input::worker_context::wrap_blocking"),
                _ => false,
            },
            _ => false,
        }
    }
    fn name(&self) -> String {
        if self.item.is_empty() {
            "<file>".into()
        } else {
            self.item.join("::")
        }
    }
    fn record(&mut self, primitive: String, tokens: TokenStream, line: usize) {
        let hash = fingerprint(tokens);
        let item = self.name();
        let occurrence = self
            .counts
            .entry((item.clone(), primitive.clone(), hash))
            .or_default();
        *occurrence += 1;
        self.sites.push(Site {
            file: self.file.clone(),
            item,
            primitive,
            fingerprint: hash,
            occurrence: *occurrence,
            line,
        });
    }
    fn with_item(&mut self, name: String, f: impl FnOnce(&mut Self)) {
        self.item.push(name);
        f(self);
        self.item.pop();
    }
    fn resolve(&self, path: &syn::Path) -> Vec<String> {
        let pieces: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if pieces.is_empty() {
            return Vec::new();
        }
        let suffix = pieces[1..].join("::");
        let join = |head: &str| {
            if suffix.is_empty() {
                head.to_owned()
            } else {
                format!("{head}::{suffix}")
            }
        };
        let mut result = vec![pieces.join("::")];
        if let Some(aliases) = self.aliases.get(&pieces[0]) {
            result.extend(aliases.iter().map(|a| join(a)));
        }
        result
    }
}

impl<'ast> Visit<'ast> for Scanner {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let attrs: &[Attribute] = match expr {
            Expr::Array(x) => &x.attrs,
            Expr::Assign(x) => &x.attrs,
            Expr::Async(x) => &x.attrs,
            Expr::Await(x) => &x.attrs,
            Expr::Binary(x) => &x.attrs,
            Expr::Block(x) => &x.attrs,
            Expr::Break(x) => &x.attrs,
            Expr::Call(x) => &x.attrs,
            Expr::Cast(x) => &x.attrs,
            Expr::Closure(x) => &x.attrs,
            Expr::Const(x) => &x.attrs,
            Expr::Continue(x) => &x.attrs,
            Expr::Field(x) => &x.attrs,
            Expr::ForLoop(x) => &x.attrs,
            Expr::Group(x) => &x.attrs,
            Expr::If(x) => &x.attrs,
            Expr::Index(x) => &x.attrs,
            Expr::Infer(x) => &x.attrs,
            Expr::Let(x) => &x.attrs,
            Expr::Lit(x) => &x.attrs,
            Expr::Loop(x) => &x.attrs,
            Expr::Macro(x) => &x.attrs,
            Expr::Match(x) => &x.attrs,
            Expr::MethodCall(x) => &x.attrs,
            Expr::Paren(x) => &x.attrs,
            Expr::Path(x) => &x.attrs,
            Expr::Range(x) => &x.attrs,
            Expr::RawAddr(x) => &x.attrs,
            Expr::Reference(x) => &x.attrs,
            Expr::Repeat(x) => &x.attrs,
            Expr::Return(x) => &x.attrs,
            Expr::Struct(x) => &x.attrs,
            Expr::Try(x) => &x.attrs,
            Expr::TryBlock(x) => &x.attrs,
            Expr::Tuple(x) => &x.attrs,
            Expr::Unary(x) => &x.attrs,
            Expr::Unsafe(x) => &x.attrs,
            Expr::While(x) => &x.attrs,
            Expr::Yield(x) => &x.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_expr(self, expr);
        }
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !test_only(&local.attrs) {
            visit::visit_local(self, local);
        }
    }
    fn visit_item(&mut self, i: &'ast syn::Item) {
        let attrs: &[Attribute] = match i {
            syn::Item::Const(x) => &x.attrs,
            syn::Item::Enum(x) => &x.attrs,
            syn::Item::ExternCrate(x) => &x.attrs,
            syn::Item::Fn(x) => &x.attrs,
            syn::Item::ForeignMod(x) => &x.attrs,
            syn::Item::Impl(x) => &x.attrs,
            syn::Item::Macro(x) => &x.attrs,
            syn::Item::Mod(x) => &x.attrs,
            syn::Item::Static(x) => &x.attrs,
            syn::Item::Struct(x) => &x.attrs,
            syn::Item::Trait(x) => &x.attrs,
            syn::Item::TraitAlias(x) => &x.attrs,
            syn::Item::Type(x) => &x.attrs,
            syn::Item::Union(x) => &x.attrs,
            syn::Item::Use(x) => &x.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_item(self, i);
        }
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        if !test_only(&i.attrs) {
            imports(&i.tree, &mut Vec::new(), &mut self.aliases);
        }
    }
    fn visit_item_mod(&mut self, i: &'ast ItemMod) {
        if !test_only(&i.attrs) && i.content.is_some() {
            self.with_item(i.ident.to_string(), |s| visit::visit_item_mod(s, i));
        }
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if !test_only(&i.attrs) {
            self.with_item(i.sig.ident.to_string(), |s| visit::visit_item_fn(s, i));
        }
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.with_item(i.ident.to_string(), |s| visit::visit_item_const(s, i));
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.with_item(i.ident.to_string(), |s| visit::visit_item_static(s, i));
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if !test_only(&i.attrs) {
            self.with_item(i.sig.ident.to_string(), |s| visit::visit_impl_item_fn(s, i));
        }
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if !test_only(&i.attrs) {
            self.with_item(i.self_ty.to_token_stream().to_string(), |s| {
                visit::visit_item_impl(s, i)
            });
        }
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*i.func {
            let names = self.resolve(&p.path);
            if names.iter().any(|name| {
                name.rsplit("::")
                    .next()
                    .is_some_and(|n| matches!(n, "spawn" | "spawn_blocking" | "spawn_local"))
            }) {
                let primitive = names
                    .iter()
                    .find(|n| n.starts_with("std::") || n.starts_with("tokio::"))
                    .unwrap_or(&names[0])
                    .clone();
                // Action latency helpers wrap blocking work themselves.
                let wrapped = names
                    .iter()
                    .any(|n| n == "crate::goat::action_latency::spawn_blocking");
                if !wrapped {
                    self.record(
                        primitive,
                        i.to_token_stream(),
                        p.path.segments.last().unwrap().ident.span().start().line,
                    );
                }
            }
        }
        if !matches!(&*i.func, Expr::Path(_)) {
            self.visit_expr(&i.func);
        }
        for arg in &i.args {
            self.visit_expr(arg);
        }
    }
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        let names = self.resolve(&p.path);
        if names.iter().any(|n| {
            n.rsplit("::")
                .next()
                .is_some_and(|last| matches!(last, "spawn" | "spawn_blocking" | "spawn_local"))
        }) {
            self.record(
                "spawn-reference".into(),
                p.to_token_stream(),
                p.path.segments.last().unwrap().ident.span().start().line,
            );
        }
        visit::visit_expr_path(self, p);
    }
    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        if (i.method == "spawn" || i.method == "spawn_blocking" || i.method == "spawn_local")
            && !self.wrapped_thread_builder(i)
        {
            self.record(
                format!(".{}", i.method),
                i.to_token_stream(),
                i.method.span().start().line,
            );
        }
        visit::visit_expr_method_call(self, i);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Token trees inside macros are opaque to syn's expression visitor.
        // Any spawn-like identifier in one is reviewed as its own site.
        fn has_spawn(stream: TokenStream) -> bool {
            stream.into_iter().any(|t| match t {
                TokenTree::Group(g) => has_spawn(g.stream()),
                TokenTree::Ident(i) => i == "spawn" || i == "spawn_blocking" || i == "spawn_local",
                _ => false,
            })
        }
        if has_spawn(m.tokens.clone()) {
            self.record(
                "macro-spawn".into(),
                m.to_token_stream(),
                m.path.segments.last().unwrap().ident.span().start().line,
            );
        }
    }
}

pub(super) fn scan(file: &str, source: &str) -> Result<Vec<Site>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    if test_only(&syntax.attrs) {
        return Ok(Vec::new());
    }
    let mut scanner = Scanner {
        file: file.into(),
        item: Vec::new(),
        aliases: BTreeMap::new(),
        counts: BTreeMap::new(),
        sites: Vec::new(),
    };
    // Collect imports throughout the file before classifying calls. Rust lets
    // an import appear after its use, including in nested modules and blocks.
    scanner.visit_file(&syntax);
    scanner.sites.clear();
    scanner.counts.clear();
    scanner.visit_file(&syntax);
    Ok(scanner.sites)
}

/// Find external files reached only through a test-only module declaration.
pub(super) fn test_only_files(root: &Path, files: &[PathBuf]) -> Result<BTreeSet<PathBuf>, String> {
    fn descend(
        root: &Path,
        file: &Path,
        inherited_test: bool,
        seen: &mut BTreeSet<(PathBuf, bool)>,
        test: &mut BTreeSet<PathBuf>,
        production: &mut BTreeSet<PathBuf>,
    ) -> Result<(), String> {
        let file = file.to_owned();
        if !seen.insert((file.clone(), inherited_test)) {
            return Ok(());
        }
        let text = std::fs::read_to_string(root.join(&file))
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let syntax = syn::parse_file(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        let effective_test = inherited_test || test_only(&syntax.attrs);
        if effective_test {
            test.insert(file.clone());
        } else {
            production.insert(file.clone());
        }
        let mut base = file.parent().unwrap_or(Path::new("")).to_path_buf();
        if !matches!(
            file.file_name().and_then(|s| s.to_str()),
            Some("mod.rs" | "lib.rs" | "main.rs")
        ) {
            base.push(file.file_stem().unwrap());
        }
        fn modules(
            items: &[syn::Item],
            base: &Path,
            source_dir: &Path,
            root: &Path,
            inherited: bool,
            seen: &mut BTreeSet<(PathBuf, bool)>,
            test: &mut BTreeSet<PathBuf>,
            production: &mut BTreeSet<PathBuf>,
        ) -> Result<(), String> {
            for item in items {
                if let syn::Item::Mod(m) = item {
                    let only_test = inherited || test_only(&m.attrs);
                    if let Some((_, nested)) = &m.content {
                        let nested_base = base.join(m.ident.to_string());
                        modules(
                            nested,
                            &nested_base,
                            &nested_base,
                            root,
                            only_test,
                            seen,
                            test,
                            production,
                        )?;
                    } else {
                        let explicit = m.attrs.iter().find(|a| a.path().is_ident("path")).and_then(
                            |a| match &a.meta {
                                Meta::NameValue(v) => match &v.value {
                                    Expr::Lit(l) => match &l.lit {
                                        syn::Lit::Str(s) => Some(s.value()),
                                        _ => None,
                                    },
                                    _ => None,
                                },
                                _ => None,
                            },
                        );
                        let candidates = if let Some(path) = explicit {
                            vec![source_dir.join(path)]
                        } else {
                            vec![
                                base.join(format!("{}.rs", m.ident)),
                                base.join(m.ident.to_string()).join("mod.rs"),
                            ]
                        };
                        if let Some(child) = candidates.into_iter().find(|p| root.join(p).is_file())
                        {
                            descend(root, &child, only_test, seen, test, production)?;
                        }
                    }
                }
            }
            Ok(())
        }
        modules(
            &syntax.items,
            &base,
            file.parent().unwrap_or(Path::new("")),
            root,
            effective_test,
            seen,
            test,
            production,
        )
    }
    let mut seen = BTreeSet::new();
    let mut test = BTreeSet::new();
    let mut production = BTreeSet::new();
    for entry in ["lib.rs", "main.rs"] {
        if root.join(entry).is_file() {
            descend(
                root,
                Path::new(entry),
                false,
                &mut seen,
                &mut test,
                &mut production,
            )?;
        }
    }
    // An orphan is scanned conservatively; no filename is a blanket exemption.
    for file in files {
        if !test.contains(file) && !production.contains(file) {
            descend(root, file, false, &mut seen, &mut test, &mut production)?;
        }
    }
    test.retain(|p| !production.contains(p));
    Ok(test)
}
