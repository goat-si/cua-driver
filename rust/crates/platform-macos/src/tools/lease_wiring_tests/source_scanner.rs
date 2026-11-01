//! Regression guard against accidentally adding an unrouted raw thread start
//! in ordinary code, not a sandbox against deliberately obfuscated code.
//! Items that can shadow the `std`, `core`, `alloc`, or `tokio` path roots and
//! code-hiding `include!` or Rust-source `include_str!` are banned rather than
//! resolved; nonliteral `include_str!` is also banned when its target is
//! unknown. Associated items such as methods and fields cannot shadow those
//! path roots and are not banned. Ambiguous inclusion scans all source files.
//! Renamed imports of raw spawn primitives (`tokio::spawn`, the
//! `tokio::task` spawn functions, `std::thread::spawn`, `std::thread::Builder`,
//! `std::thread::scope`, or globs of `tokio`, `tokio::task`, `std::thread`)
//! and any re-export of one are recorded as thread-start sites, so they fail
//! the inventory unless listed. A finding is blocking only if it is a real
//! unrouted thread start in production code or one of these banned constructs
//! goes unflagged.

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::ext::IdentExt;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ItemMod, Meta, UseTree};

// Rust raw identifiers denote the same name as their unprefixed spelling.
fn normalized_ident(ident: &syn::Ident) -> syn::Ident {
    ident.unraw()
}

fn ident_text(ident: &syn::Ident) -> String {
    normalized_ident(ident).to_string()
}

fn path_is_ident(path: &syn::Path, expected: &str) -> bool {
    path.get_ident()
        .is_some_and(|ident| ident_text(ident) == expected)
}

fn normalized_tokens(tokens: TokenStream) -> TokenStream {
    tokens
        .into_iter()
        .map(|token| match token {
            TokenTree::Ident(ident) => TokenTree::Ident(normalized_ident(&ident)),
            TokenTree::Group(group) => {
                let mut normalized =
                    proc_macro2::Group::new(group.delimiter(), normalized_tokens(group.stream()));
                normalized.set_span(group.span());
                TokenTree::Group(normalized)
            }
            other => other,
        })
        .collect()
}

fn contains_ident(tokens: TokenStream, expected: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => ident_text(&ident) == expected,
        TokenTree::Group(group) => contains_ident(group.stream(), expected),
        _ => false,
    })
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct Site {
    pub file: String,
    pub item: String,
    pub primitive: String,
    pub fingerprint: u64,
    pub occurrence: usize,
    pub line: usize,
    pub raw_import: bool,
}

fn cfg_value(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path_is_ident(path, "test") => Some(false),
        Meta::List(list)
            if path_is_ident(&list.path, "all") || path_is_ident(&list.path, "any") =>
        {
            use syn::parse::Parser;
            let args = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let values: Vec<_> = args.iter().map(cfg_value).collect();
            if path_is_ident(&list.path, "all") {
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
        Meta::List(list) if path_is_ident(&list.path, "not") => {
            syn::parse2::<Meta>(list.tokens.clone())
                .ok()
                .and_then(|m| cfg_value(&m))
                .map(|v| !v)
        }
        _ => None,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        path_is_ident(a.path(), "cfg")
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
            prefix.push(ident_text(&p.ident));
            imports(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let ident = ident_text(&n.ident);
            let mut path = prefix.clone();
            let name = if ident == "self" {
                match prefix.last() {
                    Some(name) => name.clone(),
                    None => return,
                }
            } else {
                path.push(ident.clone());
                ident
            };
            out.entry(name).or_default().insert(path.join("::"));
        }
        UseTree::Rename(n) => {
            let mut path = prefix.clone();
            path.push(ident_text(&n.ident));
            out.entry(ident_text(&n.rename))
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

type Aliases = BTreeMap<String, BTreeSet<String>>;
const LOCAL_BINDING: &str = "<local-binding>";

fn bind_pattern(aliases: &mut Aliases, pattern: &syn::Pat) {
    struct Bindings<'a>(&'a mut Aliases);
    impl<'ast> Visit<'ast> for Bindings<'_> {
        fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
            self.0
                .entry(ident_text(&pattern.ident))
                .or_default()
                .insert(LOCAL_BINDING.into());
            visit::visit_pat_ident(self, pattern);
        }
    }
    Bindings(aliases).visit_pat(pattern);
}

fn parameters(sig: &syn::Signature) -> Aliases {
    let mut result = generic_bindings(&sig.generics);
    for arg in &sig.inputs {
        if let syn::FnArg::Typed(arg) = arg {
            bind_pattern(&mut result, &arg.pat);
        }
    }
    result
}

fn bind_name(aliases: &mut Aliases, name: &syn::Ident) {
    aliases
        .entry(ident_text(name))
        .or_default()
        .insert(LOCAL_BINDING.into());
}

fn generic_bindings(generics: &syn::Generics) -> Aliases {
    let mut result = Aliases::new();
    for param in &generics.params {
        match param {
            syn::GenericParam::Type(param) => bind_name(&mut result, &param.ident),
            syn::GenericParam::Const(param) => bind_name(&mut result, &param.ident),
            syn::GenericParam::Lifetime(_) => {}
        }
    }
    result
}

fn condition_bindings(expr: &Expr) -> Aliases {
    struct Conditions(Aliases);
    impl<'ast> Visit<'ast> for Conditions {
        fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
            bind_pattern(&mut self.0, &expr.pat);
            visit::visit_expr_let(self, expr);
        }
    }
    let mut collector = Conditions(Aliases::new());
    collector.visit_expr(expr);
    collector.0
}

fn item_imports<'a>(items: impl IntoIterator<Item = &'a syn::Item>) -> Aliases {
    let mut aliases = Aliases::new();
    for item in items {
        if test_only(item_attrs(item)) {
            continue;
        }
        match item {
            syn::Item::Use(item) => imports(&item.tree, &mut Vec::new(), &mut aliases),
            syn::Item::Fn(item) => bind_name(&mut aliases, &item.sig.ident),
            syn::Item::Const(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Static(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Struct(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Enum(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Union(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Mod(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Trait(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::TraitAlias(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::Type(item) => bind_name(&mut aliases, &item.ident),
            syn::Item::ExternCrate(item) => bind_name(
                &mut aliases,
                item.rename.as_ref().map_or(&item.ident, |(_, name)| name),
            ),
            syn::Item::Macro(item) => {
                if let Some(name) = &item.ident {
                    bind_name(&mut aliases, name);
                }
            }
            syn::Item::ForeignMod(item) => {
                for foreign in &item.items {
                    match foreign {
                        syn::ForeignItem::Fn(item) if !test_only(&item.attrs) => {
                            bind_name(&mut aliases, &item.sig.ident)
                        }
                        syn::ForeignItem::Static(item) if !test_only(&item.attrs) => {
                            bind_name(&mut aliases, &item.ident)
                        }
                        syn::ForeignItem::Type(item) if !test_only(&item.attrs) => {
                            bind_name(&mut aliases, &item.ident)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    aliases
}

fn item_attrs(i: &syn::Item) -> &[Attribute] {
    match i {
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
    }
}

fn has_glob(tree: &UseTree) -> bool {
    match tree {
        UseTree::Glob(_) => true,
        UseTree::Path(path) => has_glob(&path.tree),
        UseTree::Group(group) => group.items.iter().any(has_glob),
        _ => false,
    }
}

fn reserved_path_root(name: &syn::Ident) -> Option<String> {
    let spelling = ident_text(name);
    matches!(spelling.as_str(), "std" | "core" | "alloc" | "tokio").then_some(spelling)
}

fn use_path_roots(tree: &UseTree, prefix: &mut Vec<syn::Ident>, out: &mut Vec<(String, usize)>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.clone());
            use_path_roots(&path.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let binding = if ident_text(&name.ident) == "self" {
                prefix.last()
            } else {
                Some(&name.ident)
            };
            if let Some(binding) = binding {
                if let Some(root) = reserved_path_root(binding) {
                    out.push((root, binding.span().start().line));
                }
            }
        }
        UseTree::Rename(rename) => {
            if let Some(root) = reserved_path_root(&rename.rename) {
                out.push((root, rename.rename.span().start().line));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                use_path_roots(item, prefix, out);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn item_path_roots(item: &syn::Item) -> Vec<(String, usize)> {
    let name = match item {
        syn::Item::Const(item) => Some(&item.ident),
        syn::Item::Enum(item) => Some(&item.ident),
        syn::Item::ExternCrate(item) => {
            Some(item.rename.as_ref().map_or(&item.ident, |(_, name)| name))
        }
        syn::Item::Fn(item) => Some(&item.sig.ident),
        syn::Item::Macro(item) => item.ident.as_ref(),
        syn::Item::Mod(item) => Some(&item.ident),
        syn::Item::Static(item) => Some(&item.ident),
        syn::Item::Struct(item) => Some(&item.ident),
        syn::Item::Trait(item) => Some(&item.ident),
        syn::Item::TraitAlias(item) => Some(&item.ident),
        syn::Item::Type(item) => Some(&item.ident),
        syn::Item::Union(item) => Some(&item.ident),
        _ => None,
    };
    let mut result = Vec::new();
    if let Some(name) = name {
        if let Some(root) = reserved_path_root(name) {
            result.push((root, name.span().start().line));
        }
    }
    if let syn::Item::Use(item) = item {
        use_path_roots(&item.tree, &mut Vec::new(), &mut result);
    }
    result
}

fn forbidden_include(name: &str, tokens: TokenStream) -> bool {
    match name {
        "include" => true,
        "include_str" => match syn::parse2::<syn::LitStr>(tokens) {
            Ok(path) => Path::new(&path.value())
                .extension()
                .is_some_and(|ext| ext == "rs"),
            Err(_) => true,
        },
        _ => false,
    }
}

fn contains_forbidden_include(tokens: TokenStream) -> bool {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for index in 0..tokens.len() {
        if let TokenTree::Group(group) = &tokens[index] {
            if contains_forbidden_include(group.stream()) {
                return true;
            }
        }
        if let (
            Some(TokenTree::Ident(name)),
            Some(TokenTree::Punct(bang)),
            Some(TokenTree::Group(args)),
        ) = (
            tokens.get(index),
            tokens.get(index + 1),
            tokens.get(index + 2),
        ) {
            if bang.as_char() == '!' && forbidden_include(&ident_text(name), args.stream()) {
                return true;
            }
        }
    }
    false
}

// These are scanner guard false greens, not production bypasses: the current
// production inventory has zero renamed or re-exported raw spawn primitives.
// This blunt rule records each such import as a thread-start inventory site.
fn raw_primitive_imports(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    exported: bool,
    out: &mut Vec<(String, usize)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_text(&path.ident));
            raw_primitive_imports(&path.tree, prefix, exported, out);
            prefix.pop();
        }
        UseTree::Group(group) => {
            for item in &group.items {
                raw_primitive_imports(item, prefix, exported, out);
            }
        }
        UseTree::Name(name) => {
            let mut path = prefix.clone();
            if ident_text(&name.ident) != "self" {
                path.push(ident_text(&name.ident));
            }
            if exported && potential_raw_primitive(&path) {
                out.push((path.join("::"), name.ident.span().start().line));
            }
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            if ident_text(&rename.ident) != "self" {
                path.push(ident_text(&rename.ident));
            }
            let renamed = path
                .last()
                .is_none_or(|original| ident_text(&rename.rename) != *original);
            if (exported || renamed) && potential_raw_primitive(&path) {
                out.push((path.join("::"), rename.ident.span().start().line));
            }
        }
        UseTree::Glob(glob) => {
            let path = prefix.join("::");
            if matches!(path.as_str(), "tokio" | "tokio::task" | "std::thread") {
                out.push((format!("{path}::*"), glob.star_token.span.start().line));
            }
        }
    }
}

fn potential_raw_primitive(path: &[String]) -> bool {
    if path.join("::") == "crate::goat::action_latency::spawn_blocking" {
        return false;
    }
    // Unknown prefixes can themselves be aliases or re-exports, so reject
    // potentially raw names rather than trying to infer a safe origin.
    path.last().is_some_and(|name| {
        matches!(
            name.as_str(),
            "spawn" | "spawn_blocking" | "spawn_local" | "Builder" | "scope"
        )
    })
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
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
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[Attribute] {
    match item {
        syn::TraitItem::Const(item) => &item.attrs,
        syn::TraitItem::Fn(item) => &item.attrs,
        syn::TraitItem::Type(item) => &item.attrs,
        syn::TraitItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[Attribute] {
    match item {
        syn::ImplItem::Const(item) => &item.attrs,
        syn::ImplItem::Fn(item) => &item.attrs,
        syn::ImplItem::Type(item) => &item.attrs,
        syn::ImplItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn foreign_item_attrs(item: &syn::ForeignItem) -> &[Attribute] {
    match item {
        syn::ForeignItem::Fn(item) => &item.attrs,
        syn::ForeignItem::Static(item) => &item.attrs,
        syn::ForeignItem::Type(item) => &item.attrs,
        syn::ForeignItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

#[derive(Default)]
struct BodyBindings {
    names: BTreeSet<String>,
    imports: Aliases,
}

impl<'ast> Visit<'ast> for BodyBindings {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        self.names.insert(ident_text(&pattern.ident));
        visit::visit_pat_ident(self, pattern);
    }
    fn visit_generic_param(&mut self, param: &'ast syn::GenericParam) {
        match param {
            syn::GenericParam::Type(item) => {
                self.names.insert(ident_text(&item.ident));
            }
            syn::GenericParam::Const(item) => {
                self.names.insert(ident_text(&item.ident));
            }
            syn::GenericParam::Lifetime(_) => {}
        }
        visit::visit_generic_param(self, param);
    }
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if test_only(item_attrs(item)) {
            return;
        }
        for (name, values) in item_imports(std::iter::once(item)) {
            if values.contains(LOCAL_BINDING) {
                self.names.insert(name.clone());
            }
            self.imports
                .entry(name)
                .or_default()
                .extend(values.into_iter().filter(|value| value != LOCAL_BINDING));
        }
        visit::visit_item(self, item);
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !test_only(&local.attrs) {
            visit::visit_local(self, local);
        }
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if !test_only(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if !test_only(trait_item_attrs(item)) {
            match item {
                syn::TraitItem::Fn(item) => {
                    self.names.insert(ident_text(&item.sig.ident));
                }
                syn::TraitItem::Const(item) => {
                    self.names.insert(ident_text(&item.ident));
                }
                syn::TraitItem::Type(item) => {
                    self.names.insert(ident_text(&item.ident));
                }
                _ => {}
            }
            visit::visit_trait_item(self, item);
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if !test_only(impl_item_attrs(item)) {
            match item {
                syn::ImplItem::Fn(item) => {
                    self.names.insert(ident_text(&item.sig.ident));
                }
                syn::ImplItem::Const(item) => {
                    self.names.insert(ident_text(&item.ident));
                }
                syn::ImplItem::Type(item) => {
                    self.names.insert(ident_text(&item.ident));
                }
                _ => {}
            }
            visit::visit_impl_item(self, item);
        }
    }
    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        if !test_only(foreign_item_attrs(item)) {
            visit::visit_foreign_item(self, item);
        }
    }
}

struct Scanner {
    file: String,
    item: Vec<String>,
    modules: Vec<String>,
    scopes: Vec<Aliases>,
    module_scope: usize,
    body_bindings: Vec<BodyBindings>,
    counts: BTreeMap<(String, String, u64), usize>,
    sites: Vec<Site>,
}

impl Scanner {
    fn trusted(&self, path: &syn::Path, expected: &str) -> bool {
        let Some(first) = path.segments.first() else {
            return false;
        };
        let name = ident_text(&first.ident);
        !self.body_bindings.iter().any(|scope| {
            scope.names.contains(&name)
                || scope.imports.get(&name).is_some_and(|imports| {
                    imports.iter().any(|import| {
                        expected != import
                            && !expected
                                .strip_prefix(import)
                                .is_some_and(|suffix| suffix.starts_with("::"))
                    })
                })
        }) && self.resolve(path).as_slice() == [expected]
    }

    fn wrapped_thread_builder(&self, call: &syn::ExprMethodCall) -> bool {
        fn builder(scanner: &Scanner, expr: &Expr) -> bool {
            match expr {
                Expr::MethodCall(method) => builder(scanner, &method.receiver),
                Expr::Call(call) => match &*call.func {
                    Expr::Path(path) => scanner.trusted(&path.path, "std::thread::Builder::new"),
                    _ => false,
                },
                _ => false,
            }
        }
        if ident_text(&call.method) != "spawn"
            || call.args.len() != 1
            || !builder(self, &call.receiver)
        {
            return false;
        }
        match call.args.first() {
            Some(Expr::Call(wrap)) if wrap.args.len() == 1 => match &*wrap.func {
                Expr::Path(path) => {
                    self.trusted(&path.path, "crate::input::worker_context::wrap_blocking")
                }
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
            raw_import: false,
        });
    }
    fn with_item(&mut self, name: String, f: impl FnOnce(&mut Self)) {
        self.item.push(name);
        f(self);
        self.item.pop();
    }
    fn resolve(&self, path: &syn::Path) -> Vec<String> {
        let pieces: Vec<_> = path.segments.iter().map(|s| ident_text(&s.ident)).collect();
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
        let mut local = false;
        let mut chosen = None;
        for scope in self.scopes[self.module_scope..].iter().rev() {
            if let Some(aliases) = scope.get(&pieces[0]) {
                local |= aliases.contains(LOCAL_BINDING);
                if chosen.is_none() {
                    let imports: Vec<_> = aliases
                        .iter()
                        .filter(|name| name.as_str() != LOCAL_BINDING)
                        .map(|name| join(name))
                        .collect();
                    if !imports.is_empty() {
                        chosen = Some(imports);
                    }
                }
            }
        }
        match chosen {
            Some(mut imports) => {
                if local {
                    imports.push(format!("{LOCAL_BINDING}::{}", pieces.join("::")));
                }
                imports
            }
            None => vec![pieces.join("::")],
        }
    }
}

impl<'ast> Visit<'ast> for Scanner {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if !test_only(expr_attrs(expr)) {
            if let Expr::Verbatim(tokens) = expr {
                self.record("unparsed-expression".into(), tokens.clone(), 0);
                return;
            }
            visit::visit_expr(self, expr);
        }
    }
    fn visit_pat(&mut self, pattern: &'ast syn::Pat) {
        if let syn::Pat::Verbatim(tokens) = pattern {
            self.record("unparsed-pattern".into(), tokens.clone(), 0);
        } else {
            visit::visit_pat(self, pattern);
        }
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !test_only(&local.attrs) {
            visit::visit_local(self, local);
        }
    }
    fn visit_stmt_macro(&mut self, item: &'ast syn::StmtMacro) {
        if !test_only(&item.attrs) {
            visit::visit_stmt_macro(self, item);
        }
    }
    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        let mut bindings = Aliases::new();
        for pattern in &closure.inputs {
            bind_pattern(&mut bindings, pattern);
        }
        let mut body = BodyBindings::default();
        body.visit_expr_closure(closure);
        self.body_bindings.push(body);
        self.scopes.push(bindings);
        visit::visit_expr_closure(self, closure);
        self.scopes.pop();
        self.body_bindings.pop();
    }
    fn visit_expr_for_loop(&mut self, loop_expr: &'ast syn::ExprForLoop) {
        let mut bindings = Aliases::new();
        bind_pattern(&mut bindings, &loop_expr.pat);
        self.scopes.push(bindings);
        visit::visit_expr_for_loop(self, loop_expr);
        self.scopes.pop();
    }
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        let mut bindings = Aliases::new();
        bind_pattern(&mut bindings, &arm.pat);
        self.scopes.push(bindings);
        visit::visit_arm(self, arm);
        self.scopes.pop();
    }
    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.visit_expr(&expr.cond);
        self.scopes.push(condition_bindings(&expr.cond));
        self.visit_block(&expr.then_branch);
        self.scopes.pop();
        if let Some((_, otherwise)) = &expr.else_branch {
            self.visit_expr(otherwise);
        }
    }
    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        self.visit_expr(&expr.cond);
        self.scopes.push(condition_bindings(&expr.cond));
        self.visit_block(&expr.body);
        self.scopes.pop();
    }
    fn visit_item(&mut self, i: &'ast syn::Item) {
        if !test_only(item_attrs(i)) {
            for (root, line) in item_path_roots(i) {
                self.record(
                    format!("forbidden-path-root:{root}"),
                    i.to_token_stream(),
                    line,
                );
            }
            if let syn::Item::Verbatim(tokens) = i {
                self.record("unparsed-item".into(), tokens.clone(), 0);
                return;
            }
            visit::visit_item(self, i);
        }
    }
    fn visit_trait_item(&mut self, i: &'ast syn::TraitItem) {
        if !test_only(trait_item_attrs(i)) {
            if let syn::TraitItem::Verbatim(tokens) = i {
                self.record("unparsed-trait-item".into(), tokens.clone(), 0);
                return;
            }
            visit::visit_trait_item(self, i);
        }
    }
    fn visit_impl_item(&mut self, i: &'ast syn::ImplItem) {
        if !test_only(impl_item_attrs(i)) {
            if let syn::ImplItem::Verbatim(tokens) = i {
                self.record("unparsed-impl-item".into(), tokens.clone(), 0);
                return;
            }
            visit::visit_impl_item(self, i);
        }
    }
    fn visit_foreign_item(&mut self, i: &'ast syn::ForeignItem) {
        if !test_only(foreign_item_attrs(i)) {
            let name = match i {
                syn::ForeignItem::Fn(item) => Some(&item.sig.ident),
                syn::ForeignItem::Static(item) => Some(&item.ident),
                syn::ForeignItem::Type(item) => Some(&item.ident),
                _ => None,
            };
            if let Some(name) = name {
                if let Some(root) = reserved_path_root(name) {
                    self.record(
                        format!("forbidden-path-root:{root}"),
                        i.to_token_stream(),
                        name.span().start().line,
                    );
                }
            }
            if let syn::ForeignItem::Verbatim(tokens) = i {
                self.record("unparsed-foreign-item".into(), tokens.clone(), 0);
                return;
            }
            visit::visit_foreign_item(self, i);
        }
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        if !test_only(&i.attrs) {
            let mut raw = Vec::new();
            raw_primitive_imports(
                &i.tree,
                &mut Vec::new(),
                !matches!(i.vis, syn::Visibility::Inherited),
                &mut raw,
            );
            for (path, line) in raw {
                let saved = std::mem::replace(&mut self.item, self.modules.clone());
                self.record(path, i.to_token_stream(), line);
                self.sites
                    .last_mut()
                    .expect("site just recorded")
                    .raw_import = true;
                self.item = saved;
            }
        }
        if !test_only(&i.attrs) && has_glob(&i.tree) {
            self.record(
                "glob-import".into(),
                i.to_token_stream(),
                i.use_token.span.start().line,
            );
        }
    }
    fn visit_item_mod(&mut self, i: &'ast ItemMod) {
        if let Some((_, items)) = &i.content {
            if test_only(&i.attrs) {
                return;
            }
            let previous = self.module_scope;
            self.module_scope = self.scopes.len();
            self.scopes.push(item_imports(items.iter()));
            self.modules.push(ident_text(&i.ident));
            self.with_item(ident_text(&i.ident), |s| visit::visit_item_mod(s, i));
            self.modules.pop();
            self.scopes.pop();
            self.module_scope = previous;
        }
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let mut body = BodyBindings::default();
        body.visit_block(block);
        self.body_bindings.push(body);
        let items = block.stmts.iter().filter_map(|stmt| match stmt {
            syn::Stmt::Item(item) => Some(item),
            _ => None,
        });
        let mut aliases = item_imports(items);
        for stmt in &block.stmts {
            if let syn::Stmt::Local(local) = stmt {
                if !test_only(&local.attrs) {
                    bind_pattern(&mut aliases, &local.pat);
                }
            }
        }
        self.scopes.push(aliases);
        visit::visit_block(self, block);
        self.scopes.pop();
        self.body_bindings.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if !test_only(&i.attrs) {
            let mut body = BodyBindings::default();
            body.visit_signature(&i.sig);
            body.names.insert(ident_text(&i.sig.ident));
            self.body_bindings.push(body);
            self.scopes.push(parameters(&i.sig));
            self.with_item(ident_text(&i.sig.ident), |s| visit::visit_item_fn(s, i));
            self.scopes.pop();
            self.body_bindings.pop();
        }
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.body_bindings.push(BodyBindings {
            names: [ident_text(&i.ident)].into_iter().collect(),
            ..Default::default()
        });
        self.with_item(ident_text(&i.ident), |s| visit::visit_item_const(s, i));
        self.body_bindings.pop();
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.body_bindings.push(BodyBindings {
            names: [ident_text(&i.ident)].into_iter().collect(),
            ..Default::default()
        });
        self.with_item(ident_text(&i.ident), |s| visit::visit_item_static(s, i));
        self.body_bindings.pop();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if !test_only(&i.attrs) {
            let mut body = BodyBindings::default();
            body.visit_signature(&i.sig);
            body.names.insert(ident_text(&i.sig.ident));
            self.body_bindings.push(body);
            self.scopes.push(parameters(&i.sig));
            self.with_item(ident_text(&i.sig.ident), |s| {
                visit::visit_impl_item_fn(s, i)
            });
            self.scopes.pop();
            self.body_bindings.pop();
        }
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if !test_only(&i.attrs) {
            let mut body = BodyBindings::default();
            body.visit_signature(&i.sig);
            body.names.insert(ident_text(&i.sig.ident));
            self.body_bindings.push(body);
            self.scopes.push(parameters(&i.sig));
            self.with_item(ident_text(&i.sig.ident), |s| {
                visit::visit_trait_item_fn(s, i)
            });
            self.scopes.pop();
            self.body_bindings.pop();
        }
    }
    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        if !test_only(&i.attrs) {
            self.body_bindings.push(BodyBindings {
                names: [ident_text(&i.ident)].into_iter().collect(),
                ..Default::default()
            });
            self.with_item(ident_text(&i.ident), |s| {
                visit::visit_trait_item_const(s, i)
            });
            self.body_bindings.pop();
        }
    }
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        if !test_only(&i.attrs) {
            self.body_bindings.push(BodyBindings {
                names: [ident_text(&i.ident)].into_iter().collect(),
                ..Default::default()
            });
            self.with_item(ident_text(&i.ident), |s| visit::visit_impl_item_const(s, i));
            self.body_bindings.pop();
        }
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        if !test_only(&i.attrs) {
            let names = generic_bindings(&i.generics);
            self.body_bindings.push(BodyBindings {
                names: names.keys().cloned().collect(),
                ..Default::default()
            });
            self.scopes.push(names);
            self.with_item(ident_text(&i.ident), |s| visit::visit_item_trait(s, i));
            self.scopes.pop();
            self.body_bindings.pop();
        }
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if !test_only(&i.attrs) {
            let names = generic_bindings(&i.generics);
            self.body_bindings.push(BodyBindings {
                names: names.keys().cloned().collect(),
                ..Default::default()
            });
            self.scopes.push(names);
            self.with_item(
                normalized_tokens(i.self_ty.to_token_stream()).to_string(),
                |s| visit::visit_item_impl(s, i),
            );
            self.scopes.pop();
            self.body_bindings.pop();
        }
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*i.func {
            let names = self.resolve(&p.path);
            if names.len() > 1 {
                self.record(
                    format!(
                        "ambiguous-alias: {} => {}",
                        p.path.to_token_stream(),
                        names.join(", ")
                    ),
                    i.to_token_stream(),
                    p.path.segments.last().unwrap().ident.span().start().line,
                );
            } else if names.iter().any(|name| {
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
                let wrapped = self.trusted(&p.path, "crate::goat::action_latency::spawn_blocking");
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
        if names.len() > 1
            && names.iter().any(|name| {
                name.rsplit("::").next().is_some_and(|last| {
                    matches!(
                        last,
                        "spawn" | "spawn_blocking" | "spawn_local" | "wrap_blocking"
                    )
                })
            })
        {
            self.record(
                format!(
                    "ambiguous-alias: {} => {}",
                    p.path.to_token_stream(),
                    names.join(", ")
                ),
                p.to_token_stream(),
                p.path.segments.last().unwrap().ident.span().start().line,
            );
        } else if names.iter().any(|n| {
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
        if matches!(
            ident_text(&i.method).as_str(),
            "spawn" | "spawn_blocking" | "spawn_local"
        ) && !self.wrapped_thread_builder(i)
        {
            self.record(
                format!(".{}", ident_text(&i.method)),
                i.to_token_stream(),
                i.method.span().start().line,
            );
        }
        visit::visit_expr_method_call(self, i);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let name = ident_text(&m.path.segments.last().expect("macro path").ident);
        if forbidden_include(&name, m.tokens.clone())
            || contains_forbidden_include(m.tokens.clone())
        {
            self.record(
                "forbidden-include".into(),
                m.to_token_stream(),
                m.path.segments.last().unwrap().ident.span().start().line,
            );
        }
        // Token trees inside macros are opaque to syn's expression visitor.
        // Any spawn-like identifier in one is reviewed as its own site.
        fn has_spawn(scanner: &Scanner, stream: TokenStream) -> bool {
            stream.into_iter().any(|t| match t {
                TokenTree::Group(g) => has_spawn(scanner, g.stream()),
                TokenTree::Ident(i) => {
                    let ident = ident_text(&i);
                    matches!(ident.as_str(), "spawn" | "spawn_blocking" | "spawn_local")
                        || scanner.scopes[scanner.module_scope..]
                            .iter()
                            .filter_map(|scope| scope.get(&ident))
                            .chain(
                                scanner
                                    .body_bindings
                                    .iter()
                                    .filter_map(|scope| scope.imports.get(&ident)),
                            )
                            .any(|paths| {
                                paths.iter().any(|path| {
                                    path.rsplit("::").next().is_some_and(|name| {
                                        matches!(
                                            name,
                                            "spawn"
                                                | "spawn_blocking"
                                                | "spawn_local"
                                                | "wrap_blocking"
                                        )
                                    })
                                })
                            })
                }
                _ => false,
            })
        }
        if has_spawn(self, m.path.to_token_stream()) || has_spawn(self, m.tokens.clone()) {
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
        modules: Vec::new(),
        scopes: vec![item_imports(syntax.items.iter())],
        module_scope: 0,
        body_bindings: Vec::new(),
        counts: BTreeMap::new(),
        sites: Vec::new(),
    };
    scanner.visit_file(&syntax);
    Ok(scanner.sites)
}

/// Find external files reached only through a test-only module declaration.
pub(super) fn test_only_files(root: &Path, files: &[PathBuf]) -> Result<BTreeSet<PathBuf>, String> {
    fn normalize(root: &Path, file: &Path) -> Result<PathBuf, String> {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?;
        let absolute = root
            .join(file)
            .canonicalize()
            .map_err(|e| format!("{}: {e}", file.display()))?;
        absolute
            .strip_prefix(&canonical_root)
            .map(Path::to_path_buf)
            .map_err(|_| format!("module path escapes source root: {}", file.display()))
    }

    #[derive(Default)]
    struct ReachabilityRisk {
        block_depth: usize,
        uncertain: bool,
    }
    impl<'ast> Visit<'ast> for ReachabilityRisk {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            if !test_only(item_attrs(item)) {
                visit::visit_item(self, item);
            }
        }
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if !test_only(expr_attrs(expr)) {
                visit::visit_expr(self, expr);
            }
        }
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if !test_only(&local.attrs) {
                visit::visit_local(self, local);
            }
        }
        fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
            if !test_only(trait_item_attrs(item)) {
                visit::visit_trait_item(self, item);
            }
        }
        fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
            if !test_only(impl_item_attrs(item)) {
                visit::visit_impl_item(self, item);
            }
        }
        fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
            if !test_only(foreign_item_attrs(item)) {
                visit::visit_foreign_item(self, item);
            }
        }
        fn visit_stmt_macro(&mut self, item: &'ast syn::StmtMacro) {
            if !test_only(&item.attrs) {
                visit::visit_stmt_macro(self, item);
            }
        }
        fn visit_block(&mut self, block: &'ast syn::Block) {
            self.block_depth += 1;
            visit::visit_block(self, block);
            self.block_depth -= 1;
        }
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if test_only(&item.attrs) {
                return;
            }
            if item.content.is_none() && self.block_depth > 0 {
                self.uncertain = true;
            }
            if item.attrs.iter().any(|attr| {
                path_is_ident(attr.path(), "cfg_attr")
                    && contains_ident(attr.to_token_stream(), "path")
            }) {
                self.uncertain = true;
            }
            visit::visit_item_mod(self, item);
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            fn nested_mod(tokens: TokenStream) -> bool {
                let tokens: Vec<_> = tokens.into_iter().collect();
                (0..tokens.len()).any(|index| {
                    matches!(tokens.get(index), Some(TokenTree::Ident(ident)) if ident_text(ident) == "mod")
                        && matches!(tokens.get(index + 1), Some(TokenTree::Ident(_)))
                        && matches!(tokens.get(index + 2), Some(TokenTree::Punct(punct)) if punct.as_char() == ';')
                        || matches!(tokens.get(index), Some(TokenTree::Group(group)) if nested_mod(group.stream()))
                })
            }
            self.uncertain |= nested_mod(m.tokens.clone());
        }
    }

    fn descend(
        root: &Path,
        file: &Path,
        inherited_test: bool,
        seen: &mut BTreeSet<(PathBuf, bool)>,
        test: &mut BTreeSet<PathBuf>,
        production: &mut BTreeSet<PathBuf>,
    ) -> Result<(), String> {
        let file = normalize(root, file)?;
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
        struct ModuleDirs<'a> {
            base: &'a Path,
            source: &'a Path,
        }
        fn modules(
            items: &[syn::Item],
            dirs: ModuleDirs<'_>,
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
                        let nested_base = dirs.base.join(ident_text(&m.ident));
                        modules(
                            nested,
                            ModuleDirs {
                                base: &nested_base,
                                source: &nested_base,
                            },
                            root,
                            only_test,
                            seen,
                            test,
                            production,
                        )?;
                    } else {
                        let explicit = m
                            .attrs
                            .iter()
                            .find(|a| path_is_ident(a.path(), "path"))
                            .and_then(|a| match &a.meta {
                                Meta::NameValue(v) => match &v.value {
                                    Expr::Lit(l) => match &l.lit {
                                        syn::Lit::Str(s) => Some(s.value()),
                                        _ => None,
                                    },
                                    _ => None,
                                },
                                _ => None,
                            });
                        let candidates = if let Some(path) = explicit {
                            vec![dirs.source.join(path)]
                        } else {
                            vec![
                                dirs.base.join(format!("{}.rs", ident_text(&m.ident))),
                                dirs.base.join(ident_text(&m.ident)).join("mod.rs"),
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
            ModuleDirs {
                base: &base,
                source: file.parent().unwrap_or(Path::new("")),
            },
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
    for file in &production {
        let text = std::fs::read_to_string(root.join(file))
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let sites =
            scan(&file.to_string_lossy(), &text).map_err(|e| format!("{}: {e}", file.display()))?;
        let syntax = syn::parse_file(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        let mut risk = ReachabilityRisk::default();
        risk.visit_file(&syntax);
        if risk.uncertain
            || sites
                .iter()
                .any(|site| site.primitive == "forbidden-include")
        {
            // We cannot safely prove which sources are test-only.
            return Ok(BTreeSet::new());
        }
    }
    test.retain(|p| !production.contains(p));
    Ok(test)
}
