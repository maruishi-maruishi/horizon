//! Generates the bridge between the game's native mod API and WebAssembly plugins.
//!
//! The game's own build generates `Hooks` (one method per hook, each with its event struct) and
//! `Host` (the helpers) from the hook catalog and the game's sources. This script runs the same
//! generator, reads its output with syn and writes:
//!
//! - `bridge_hooks.rs`: `impl Hooks for WasmHost`, one method per hook whose host is `Host`. Each
//!   sends its event's fields to the plugins as JSON (fields of types JSON can't carry are left
//!   out), writes back the `&mut` fields a plugin set, and turns the reply into the hook's
//!   result (`Flow` for a cancellable hook, `Option<T>` for a replacing one).
//! - `bridge_host.rs`: `call_host`, which runs a `Host` helper by name with JSON arguments, for
//!   every helper whose arguments and result JSON can carry.
//!
//! A new hook or helper in the game reaches the plugins with no change here.
use quote::{format_ident, quote};
use std::{env, fs, path::PathBuf};
use syn::{FnArg, GenericArgument, Item, Pat, PathArguments, ReturnType, TraitItem, Type};

#[path = "../../packaging/windows/exe.rs"]
mod windows_exe;

/// Hooks the plugin host answers itself (inherent `custom_<name>` methods in lib.rs).
const CUSTOM: &[&str] = &[
    "register_commands",
    "perform_command",
    "perform_console_command",
    "permission_check",
    "permission_groups",
    "server_tick",
    "player_logged_in",
    "player_command",
    "player_profile",
];

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let api = out.join("api");
    fs::create_dir_all(&api).unwrap();
    horizon_mod_api_gen::generate(&root, &api, horizon_mod_api_gen::Target::Client)
        .expect("generating the native mod API");
    let hooks = syn::parse_file(&fs::read_to_string(api.join("mod_api_generated.rs")).unwrap())
        .expect("parsing the generated hooks");
    let host = syn::parse_file(&fs::read_to_string(api.join("mod_api_generated_host.rs")).unwrap())
        .expect("parsing the generated host");
    fs::write(out.join("bridge_hooks.rs"), bridge_hooks(&hooks)).unwrap();
    fs::write(out.join("bridge_host.rs"), bridge_host(&host)).unwrap();
    windows_exe::resources("horizon-plugin-server");
}

/// The last path segment's name and its generic arguments.
fn last(ty: &Type) -> Option<(String, Vec<&Type>)> {
    let Type::Path(p) = ty else { return None };
    if p.qself.is_some() || p.path.segments.len() != 1 {
        return None;
    }
    let seg = p.path.segments.last()?;
    let args = match &seg.arguments {
        PathArguments::None => Vec::new(),
        PathArguments::AngleBracketed(a) => a
            .args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
        PathArguments::Parenthesized(_) => return None,
    };
    Some((seg.ident.to_string(), args))
}

const NUMBERS: &[&str] = &[
    "bool", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "u128", "usize", "f32", "f64",
];

/// Whether JSON carries `ty` one way: `to` from the game to a plugin, else back.
fn carried(ty: &Type, to: bool) -> bool {
    match ty {
        Type::Array(a) => {
            last(&a.elem).is_some_and(|(n, g)| g.is_empty() && NUMBERS.contains(&n.as_str()))
        }
        Type::Tuple(t) => t.elems.len() <= 3 && t.elems.iter().all(|e| carried(e, to)),
        Type::Paren(p) => carried(&p.elem, to),
        _ => match last(ty) {
            Some((name, generics)) => match name.as_str() {
                n if NUMBERS.contains(&n) => generics.is_empty(),
                "String" | "Text" | "BlockStateId" => generics.is_empty(),
                "NativeServerPlayer" => to && generics.is_empty(),
                "Option" | "Vec" => generics.len() == 1 && carried(generics[0], to),
                "Result" => to && generics.len() == 2 && generics.iter().all(|g| carried(g, true)),
                _ => false,
            },
            None => false,
        },
    }
}

enum Field<'a> {
    Owned(&'a Type),
    Shared(&'a Type),
    Mutable(&'a Type),
}

fn field(ty: &Type) -> Field<'_> {
    match ty {
        Type::Reference(r) if r.mutability.is_some() => Field::Mutable(&r.elem),
        Type::Reference(r) => Field::Shared(&r.elem),
        other => Field::Owned(other),
    }
}

fn is_str(ty: &Type) -> bool {
    matches!(last(ty), Some((n, g)) if n == "str" && g.is_empty())
}

fn shared_carried(ty: &Type) -> bool {
    is_str(ty) || matches!(ty, Type::Slice(s) if carried(&s.elem, true)) || carried(ty, true)
}

fn bridge_hooks(file: &syn::File) -> String {
    let mut structs = std::collections::HashMap::new();
    let mut trait_items = None;
    for item in &file.items {
        match item {
            Item::Struct(s) => {
                structs.insert(s.ident.to_string(), s.clone());
            }
            Item::Trait(t) if t.ident == "Hooks" => trait_items = Some(t.items.clone()),
            _ => {}
        }
    }
    let mut trait_methods = Vec::new();
    let mut inherent = Vec::new();
    let mut names = Vec::new();
    let mut fields_doc = Vec::new();
    for item in trait_items.expect("no Hooks trait") {
        let TraitItem::Fn(f) = item else { continue };
        let name = f.sig.ident.to_string();
        let inputs: Vec<_> = f.sig.inputs.iter().collect();
        // (&self, host, event) or (&self, event): hooks with no `Host` give plugins no helpers.
        let (host_arg, event_arg) = match inputs.as_slice() {
            [_, FnArg::Typed(h), FnArg::Typed(e)] => (Some(h), e),
            [_, FnArg::Typed(e)] => (None, e),
            _ => continue,
        };
        let Pat::Ident(event_name) = &*event_arg.pat else {
            continue;
        };
        let event_name = &event_name.ident;
        let (host_expr, host_param, host_pass) = match host_arg {
            Some(host_arg) => {
                let Pat::Ident(host_name) = &*host_arg.pat else {
                    continue;
                };
                let host_name = &host_name.ident;
                let Type::Reference(host_ref) = &*host_arg.ty else {
                    continue;
                };
                let host_ok = matches!(&*host_ref.elem, Type::TraitObject(o) if quote!(#o).to_string() == "dyn Host");
                if !host_ok {
                    continue;
                }
                let host_ty = &host_arg.ty;
                let expr = if host_ref.mutability.is_some() {
                    quote!(HostRef::Mut(&mut *#host_name))
                } else {
                    quote!(HostRef::Shared(#host_name))
                };
                (expr, quote!(#host_name: #host_ty,), quote!(#host_name,))
            }
            None => (quote!(HostRef::None), quote!(), quote!()),
        };
        let Type::Reference(event_ref) = &*event_arg.ty else {
            continue;
        };
        let Some((event_type, _)) = last(&event_ref.elem) else {
            continue;
        };
        let Some(event_struct) = structs.get(&event_type) else {
            continue;
        };
        // The result.
        enum Ret {
            Unit,
            Flow,
            Replace,
        }
        let ret = match &f.sig.output {
            ReturnType::Default => Ret::Unit,
            ReturnType::Type(_, ty) => match last(ty) {
                Some((n, g)) if n == "Flow" && g.is_empty() => Ret::Flow,
                Some((n, g)) if n == "Option" && g.len() == 1 && carried(g[0], false) => {
                    Ret::Replace
                }
                _ => continue,
            },
        };
        let sig = &f.sig;
        if CUSTOM.contains(&name.as_str()) {
            let custom = format_ident!("custom_{}", name);
            trait_methods.push(quote! { #sig { self.#custom(#host_pass #event_name) } });
            // Custom hooks still reach the plugins through `forward_<name>`.
        }
        let index = names.len();
        let mut inserts = Vec::new();
        let mut sets = Vec::new();
        let mut carried_fields = Vec::new();
        if let syn::Fields::Named(named) = &event_struct.fields {
            for f in &named.named {
                let ident = f.ident.as_ref().unwrap();
                let key = ident.to_string();
                match field(&f.ty) {
                    Field::Owned(t) if carried(t, true) => {
                        inserts.push(quote! { data.insert(#key.into(), ToJson::to_json(&#event_name.#ident)); });
                    }
                    Field::Shared(t) if shared_carried(t) => {
                        inserts.push(quote! { data.insert(#key.into(), ToJson::to_json(#event_name.#ident)); });
                    }
                    Field::Mutable(t) if carried(t, true) => {
                        inserts.push(quote! { data.insert(#key.into(), ToJson::to_json(&*#event_name.#ident)); });
                        if carried(t, false) {
                            sets.push(quote! {
                                if let Some(value) = reply.set.get(#key) {
                                    match FromJson::from_json(value) {
                                        Ok(v) => *#event_name.#ident = v,
                                        Err(e) => log::warn!("plugin set {}.{}: {e}", #name, #key),
                                    }
                                }
                            });
                        }
                    }
                    _ => continue,
                }
                carried_fields.push(key);
            }
        }
        names.push(name.clone());
        fields_doc.push(carried_fields.join(","));
        let forward = format_ident!("forward_{}", name);
        let result = match ret {
            Ret::Unit => quote! {},
            Ret::Flow => quote! { if reply.cancel { Flow::Cancel } else { Flow::Continue } },
            Ret::Replace => {
                quote! { reply.value.as_ref().and_then(|v| FromJson::from_json(v).ok()) }
            }
        };
        let empty = match ret {
            Ret::Unit => quote! { return; },
            Ret::Flow => quote! { return Flow::Continue; },
            Ret::Replace => quote! { return None; },
        };
        let out = match ret {
            Ret::Unit => quote! {},
            Ret::Flow => quote! { -> Flow },
            Ret::Replace => match &f.sig.output {
                ReturnType::Type(_, ty) => quote! { -> #ty },
                ReturnType::Default => unreachable!(),
            },
        };
        let event_ty = &event_arg.ty;
        let body = quote! {
            if !self.wants(#index) { #empty }
            #[allow(unused_mut)]
            let mut data = serde_json::Map::new();
            #(#inserts)*
            let reply = self.dispatch(#host_expr, #index, serde_json::Value::Object(data));
            #(#sets;)*
            #result
        };
        // `forward_<name>`: the plugins' side of the hook, which a custom handler calls too.
        let generics = &f.sig.generics;
        inherent.push(quote! {
            #[allow(clippy::needless_lifetimes, unused_variables, dead_code)]
            pub(crate) fn #forward #generics (&self, #host_param #event_name: #event_ty) #out { #body }
        });
        if !CUSTOM.contains(&name.as_str()) {
            trait_methods.push(quote! { #sig { self.#forward(#host_pass #event_name) } });
        }
    }
    let count = names.len();
    let code = quote! {
        /// Every hook plugins can hear, by index.
        pub const HOOK_NAMES: [&str; #count] = [#(#names),*];
        /// The event fields each hook carries, comma separated.
        pub const HOOK_FIELDS: [&str; #count] = [#(#fields_doc),*];
        impl WasmHost { #(#inherent)* }
        impl Hooks for WasmHost { #(#trait_methods)* }
    };
    format!("// @generated by horizon-wasm-plugins/build.rs\n{code}")
}

fn bridge_host(file: &syn::File) -> String {
    let mut arms = Vec::new();
    let mut names = Vec::new();
    for item in &file.items {
        let Item::Trait(t) = item else { continue };
        if t.ident != "Host" {
            continue;
        }
        'methods: for item in &t.items {
            let TraitItem::Fn(f) = item else { continue };
            if !f.sig.generics.params.is_empty() || f.sig.asyncness.is_some() {
                continue;
            }
            let name = f.sig.ident.to_string();
            let ident = &f.sig.ident;
            let mut mutable = false;
            let mut lets = Vec::new();
            let mut call_args = Vec::new();
            let mut position = 0usize;
            for input in &f.sig.inputs {
                match input {
                    FnArg::Receiver(r) => mutable = r.mutability.is_some(),
                    FnArg::Typed(t) => {
                        let var = format_ident!("a{}", position);
                        let ty = &*t.ty;
                        match ty {
                            Type::Reference(r) if r.mutability.is_none() && is_str(&r.elem) => {
                                lets.push(quote! { let #var: String = arg(args, #position)?; });
                                call_args.push(quote! { &#var });
                            }
                            Type::Reference(r) if r.mutability.is_none() => match &*r.elem {
                                Type::Slice(s) if carried(&s.elem, false) => {
                                    let elem = &s.elem;
                                    lets.push(
                                        quote! { let #var: Vec<#elem> = arg(args, #position)?; },
                                    );
                                    call_args.push(quote! { &#var[..] });
                                }
                                inner if carried(inner, false) => {
                                    lets.push(quote! { let #var: #inner = arg(args, #position)?; });
                                    call_args.push(quote! { &#var });
                                }
                                _ => continue 'methods,
                            },
                            other if carried(other, false) => {
                                lets.push(quote! { let #var: #other = arg(args, #position)?; });
                                call_args.push(quote! { #var });
                            }
                            _ => continue 'methods,
                        }
                        position += 1;
                    }
                }
            }
            let returns = match &f.sig.output {
                ReturnType::Default => true,
                ReturnType::Type(_, ty) => carried(ty, true),
            };
            if !returns {
                continue;
            }
            let target = if mutable {
                quote!(host.mutable()?)
            } else {
                quote!(host.shared()?)
            };
            arms.push(quote! {
                #name => { #(#lets)* Ok(ToJson::to_json(&#target.#ident(#(#call_args),*))) }
            });
            names.push(name);
        }
    }
    let count = names.len();
    let code = quote! {
        /// Every `Host` helper plugins can call, by name.
        pub const HOST_FUNCTIONS: [&str; #count] = [#(#names),*];
        /// Run the `Host` helper `name` with `args`.
        #[allow(clippy::unit_arg, clippy::let_unit_value)]
        pub(crate) fn call_host(host: &mut HostRef<'_>, name: &str, args: &[serde_json::Value]) -> Result<serde_json::Value, String> {
            match name {
                #(#arms)*
                _ => Err(format!("unknown host function `{name}`")),
            }
        }
    };
    format!("// @generated by horizon-wasm-plugins/build.rs\n{code}")
}
