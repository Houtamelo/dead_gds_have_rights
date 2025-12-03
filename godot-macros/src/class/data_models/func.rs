/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::class::RpcAttr;
use crate::util::{bail_fn, ident, safe_ident};
use crate::{util, ParseResult};
use proc_macro2::{Group, Ident, TokenStream, TokenTree};
use quote::{format_ident, quote};

/// Information used for registering a Rust function with Godot.
pub struct FuncDefinition {
    /// Refined signature, with higher level info and renamed parameters.
    pub signature_info: SignatureInfo,

    /// The function's non-gdext attributes (all except #[func]).
    pub external_attributes: Vec<venial::Attribute>,

    /// The name the function will be exposed as in Godot. If `None`, the Rust function name is used.
    ///
    /// This can differ from the name in [`signature_info`] if the user has used `#[func(rename)]` or for script-virtual functions.
    pub registered_name: Option<String>,

    /// True for script-virtual functions.
    pub is_script_virtual: bool,

    /// Information about the RPC configuration, if provided.
    pub rpc_info: Option<RpcAttr>,
}

impl FuncDefinition {
    pub fn rust_ident(&self) -> &Ident {
        &self.signature_info.method_name
    }

    pub fn godot_name(&self) -> String {
        if let Some(name_override) = self.registered_name.as_ref() {
            name_override.clone()
        } else {
            self.rust_ident().to_string()
        }
    }
}

/// Returns a C function which acts as the callback when a virtual method of this instance is invoked.
//
// Virtual methods are non-static by their nature; so there's no support for static ones.
pub fn make_virtual_callback(
    class_name: &Ident,
    signature_info: &SignatureInfo,
    before_kind: BeforeKind,
    interface_trait: Option<&venial::TypeExpr>,
) -> TokenStream {
    let method_name = &signature_info.method_name;

    let wrapped_method =
        make_forwarding_closure(class_name, signature_info, before_kind, interface_trait);
    let sig_params = signature_info.param_types_tuple();
    let sig_ret = &signature_info.return_type;

    let call_ctx = make_call_context(
        class_name.to_string().as_str(),
        method_name.to_string().as_str(),
    );
    let invocation = make_ptrcall_invocation(&wrapped_method, true);

    quote! {
        {
            use ::godot::sys;
            type CallParams = #sig_params;
            type CallRet = #sig_ret;

            unsafe extern "C" fn virtual_fn(
                instance_ptr: sys::GDExtensionClassInstancePtr,
                args_ptr: *const sys::GDExtensionConstTypePtr,
                ret: sys::GDExtensionTypePtr,
            ) {
                let call_ctx = #call_ctx;
                let _success = ::godot::private::handle_ptrcall_panic(
                    &call_ctx,
                    || #invocation
                );
            }
            Some(virtual_fn)
        }
    }
}

/// Generates code that registers the specified method for the given class.
pub fn make_method_registration(
    class_name: &Ident,
    func_definition: FuncDefinition,
    interface_trait: Option<&venial::TypeExpr>,
) -> ParseResult<TokenStream> {
    let signature_info = &func_definition.signature_info;
    let sig_params = signature_info.param_types_tuple();
    let sig_std_params = signature_info.std_param_types_tuple();
    let sig_ret = &signature_info.return_type;

    let is_script_virtual = func_definition.is_script_virtual;
    let method_flags = match make_method_flags(signature_info.receiver_type, is_script_virtual) {
        Ok(mf) => mf,
        Err(msg) => return bail_fn(msg, &signature_info.method_name),
    };

    let forwarding_closure = make_forwarding_closure(
        class_name,
        signature_info,
        BeforeKind::Without,
        interface_trait,
    );

    // String literals
    let class_name_str = class_name.to_string();
    let method_name_str = func_definition.godot_name();

    let call_ctx = make_call_context(&class_name_str, &method_name_str);
    let varcall_fn_decl = make_varcall_fn(&call_ctx, &forwarding_closure, &func_definition);
    let ptrcall_fn_decl = make_ptrcall_fn(&call_ctx, &forwarding_closure);

    // String literals II
    let param_ident_strs = signature_info.param_idents().map(|ident| ident.to_string());

    // Transport #[cfg] attrs to the FFI glue to ensure functions which were conditionally
    // removed from compilation don't cause errors.
    let cfg_attrs = util::extract_cfg_attrs(&func_definition.external_attributes)
        .into_iter()
        .collect::<Vec<_>>();

    let default_types = signature_info.default_params.iter().map(|(p, _)| &p.ty);
    let default_values = signature_info.default_params.iter().map(|(_, expr)| expr);

    let registration = quote! {
        #(#cfg_attrs)*
        {
            use ::godot::obj::GodotClass;
            use ::godot::register::private::method::ClassMethodInfo;
            use ::godot::builtin::{StringName, Variant};
            use ::godot::sys;
            use ::godot::meta::{ParamTuple, InParamTuple};

            type CallParams = #sig_params;
            type CallStdParams = #sig_std_params;
            type CallRet = #sig_ret;

            let method_name = StringName::from(#method_name_str);

            #varcall_fn_decl;
            #ptrcall_fn_decl;

            // SAFETY: varcall_fn + ptrcall_fn interpret their in/out parameters correctly.
            let mut method_info = unsafe {
                ClassMethodInfo::from_signature::<#class_name, CallParams, CallRet>(
                    method_name,
                    Some(varcall_fn),
                    Some(ptrcall_fn),
                    #method_flags,
                    &[
                        #( #param_ident_strs ),*
                    ],
                )
            };

            #({
                let default_val: #default_types = #default_values;
                method_info.default_arguments.push(default_val.to_variant());
            })*

            ::godot::private::out!(
                "   Register fn:   {}::{}",
                #class_name_str,
                #method_name_str
            );

            // Note: information whether the method is virtual is stored in method method_info's flags.
            method_info.register_extension_class_method();
        };
    };

    Ok(registration)
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Implementation

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ReceiverType {
    Ref,
    Mut,
    GdSelf,
    Static,
}

#[derive(Debug)]
pub struct ParamInfo {
    pub ident: Ident,
    /// Parameter types *without* receiver.
    pub ty: venial::TypeExpr,
    /// Only for changed parameters; empty if no changes.
    pub modified_ty: Option<(usize, venial::TypeExpr)>,
}

#[derive(Debug)]
pub struct SignatureInfo {
    pub method_name: Ident,
    pub receiver_type: ReceiverType,
    pub std_params: Vec<ParamInfo>,
    pub default_params: Vec<(ParamInfo, TokenStream)>,
    pub return_type: TokenStream,
}

impl SignatureInfo {
    pub fn fn_ready() -> Self {
        Self {
            method_name: ident("ready"),
            receiver_type: ReceiverType::Mut,
            std_params: vec![],
            default_params: vec![],
            return_type: quote! { () },
        }
    }

    pub fn param_types_tuple(&self) -> TokenStream {
        let std_params = self.std_params.iter().map(|p| &p.ty);
        let default_params = self.default_params.iter().map(|(p, _)| &p.ty);
        quote! { (#(#std_params,)* #(#default_params,)*) }
    }

    pub fn std_param_types_tuple(&self) -> TokenStream {
        let std_params = self.std_params.iter().map(|p| &p.ty);
        quote! { ( #(#std_params,)* ) }
    }

    pub fn param_idents(&self) -> impl Iterator<Item = &Ident> {
        self.std_params
            .iter()
            .map(|p| &p.ident)
            .chain(self.default_params.iter().map(|(p, _)| &p.ident))
    }
}

#[derive(Copy, Clone)]
pub enum BeforeKind {
    /// Default: just call the method.
    Without,

    /// Call `before_{method}` before calling the method itself.
    WithBefore,

    /// Call **only** `before_{method}`, not the method itself.
    OnlyBefore,
}

/// Returns a closure expression that forwards the parameters to the Rust instance.
fn make_forwarding_closure(
    class_name: &Ident,
    signature_info: &SignatureInfo,
    before_kind: BeforeKind,
    interface_trait: Option<&venial::TypeExpr>,
) -> TokenStream {
    let method_name = &signature_info.method_name;
    let params = signature_info.param_idents().collect::<Vec<_>>();

    let instance_decl = match &signature_info.receiver_type {
        ReceiverType::Ref => quote! {
            let instance = ::godot::private::Storage::get(storage);
        },
        ReceiverType::Mut => quote! {
            let mut instance = ::godot::private::Storage::get_mut(storage);
        },
        _ => quote! {},
    };

    let before_method_call = match before_kind {
        BeforeKind::WithBefore | BeforeKind::OnlyBefore => {
            let before_method = format_ident!("__before_{}", method_name);
            quote! { instance.#before_method(); }
        }
        BeforeKind::Without => TokenStream::new(),
    };

    match signature_info.receiver_type {
        ReceiverType::Ref | ReceiverType::Mut => {
            // Generated default virtual methods (e.g. for ready) may not have an actual implementation (user code), so
            // all they need to do is call the __before_ready() method. This means the actual method call may be optional.
            let method_call = if matches!(before_kind, BeforeKind::OnlyBefore) {
                TokenStream::new()
            } else {
                match interface_trait {
                    // impl ITrait for Class {...}
                    Some(interface_trait) => {
                        let instance_ref = match signature_info.receiver_type {
                            ReceiverType::Ref => quote! { &instance },
                            ReceiverType::Mut => quote! { &mut instance },
                            _ => unreachable!("unexpected receiver type"), // checked above.
                        };

                        quote! { <#class_name as #interface_trait>::#method_name( #instance_ref, #(#params),* ) }
                    }

                    // impl Class {...}
                    None => quote! { instance.#method_name( #(#params),* ) },
                }
            };

            quote! {
                |instance_ptr, params| {
                    let ( #(#params,)* ) = params;

                    let storage =
                        unsafe { ::godot::private::as_storage::<#class_name>(instance_ptr) };

                    #instance_decl
                    #before_method_call
                    #method_call
                }
            }
        }
        ReceiverType::GdSelf => {
            // Method call is always present, since GdSelf implies that the user declares the method.
            // (Absent method is only used in the case of a generated default virtual method, e.g. for ready()).
            quote! {
                |instance_ptr, params| {
                    let ( #(#params,)* ) = params;

                    let storage =
                        unsafe { ::godot::private::as_storage::<#class_name>(instance_ptr) };

                    #before_method_call
                    #class_name::#method_name(::godot::private::Storage::get_gd(storage), #(#params),*)
                }
            }
        }
        ReceiverType::Static => {
            // No before-call needed, since static methods are not virtual.
            quote! {
                |_, params| {
                    let ( #(#params,)* ) = params;
                    #class_name::#method_name(#(#params),*)
                }
            }
        }
    }
}

/// Maps each usage of `Self` to the struct it's referencing,
/// since `Self` can't be used inside nested functions.
fn map_self_to_class_name<In, Out>(tokens: In, class_name: &Ident) -> Out
where
    In: IntoIterator<Item = TokenTree>,
    Out: FromIterator<TokenTree>,
{
    tokens
        .into_iter()
        .map(|tt| match tt {
            // Change instances of Self to the class name.
            TokenTree::Ident(ident) if ident == "Self" => TokenTree::Ident(class_name.clone()),
            // Recurse into groups and make sure ALL instances are changed.
            TokenTree::Group(group) => TokenTree::Group(Group::new(
                group.delimiter(),
                map_self_to_class_name(group.stream(), class_name),
            )),
            // Pass all other tokens through unchanged.
            tt => tt,
        })
        .collect()
}

pub(crate) fn into_signature_info(
    signature: venial::Function,
    class_name: &Ident,
    has_gd_self: bool,
) -> SignatureInfo {
    let method_name = signature.name.clone();
    let mut receiver_type = if has_gd_self {
        ReceiverType::GdSelf
    } else {
        ReceiverType::Static
    };

    let num_params = signature.params.inner.len();
    let mut std_params = Vec::with_capacity(num_params);
    let mut default_params = Vec::new();

    let return_type = match signature.return_ty {
        None => quote! { () },
        Some(ty) => map_self_to_class_name(ty.tokens, class_name),
    };

    let mut found_default = false;

    let mut next_unnamed_index = 0;
    for (index, (arg, _)) in signature.params.inner.into_iter().enumerate() {
        match arg {
            venial::FnParam::Receiver(recv) => {
                if receiver_type == ReceiverType::GdSelf {
                    // This shouldn't happen, as when has_gd_self is true the first function parameter should have been removed.
                    // And the first parameter should be the only one that can be a Receiver.
                    panic!("has_gd_self is true for a signature starting with a Receiver param.");
                }
                receiver_type = if recv.tk_mut.is_some() {
                    ReceiverType::Mut
                } else if recv.tk_ref.is_some() {
                    ReceiverType::Ref
                } else {
                    panic!("Receiver not supported");
                };
            }
            venial::FnParam::Typed(arg) => {
                let ident = maybe_rename_parameter(arg.name, &mut next_unnamed_index);
                let (ty, modified_ty) =
                    match maybe_change_parameter_type(arg.ty, &method_name, index) {
                        // Parameter type was modified.
                        Ok(ty) => (ty.clone(), Some((index, ty))),

                        // Not an error, just unchanged.
                        Err(ty) => {
                            let ty = venial::TypeExpr {
                                tokens: map_self_to_class_name(ty.tokens, class_name),
                            };
                            (ty, None)
                        }
                    };

                let param_info = ParamInfo {
                    ident,
                    ty,
                    modified_ty,
                };

                let default_expr = arg.attributes.iter().find_map(|attr| {
                    let ident = attr.path.first().and_then(|t| {
                        if let TokenTree::Ident(id) = t {
                            Some(id)
                        } else {
                            None
                        }
                    })?;

                    if ident == "default" {
                        let expr_tokens = attr.get_value_tokens();
                        Some(quote! { #(#expr_tokens)* })
                    } else {
                        None
                    }
                });

                match (default_expr, found_default) {
                    (Some(expr), _) => {
                        found_default = true;
                        default_params.push((param_info, expr));
                    }
                    (None, true) => {
                        // All default parameters must come after non-default ones.
                        panic!("Non-default parameter cannot follow default parameters");
                    }
                    (None, false) => std_params.push(param_info),
                }
            }
        }
    }

    SignatureInfo {
        method_name,
        receiver_type,
        std_params,
        default_params,
        return_type,
    }
}

/// If `f32` is used for a delta parameter in a virtual process function, transparently use `f64` behind the scenes.
fn maybe_change_parameter_type(
    param_ty: venial::TypeExpr,
    method_name: &Ident,
    param_index: usize,
) -> Result<venial::TypeExpr, venial::TypeExpr> {
    // A bit hackish, but TokenStream APIs are also notoriously annoying to work with. Not even PartialEq...

    if param_index == 1
        && (method_name == "process" || method_name == "physics_process")
        && param_ty.tokens.len() == 1
        && param_ty.tokens[0].to_string() == "f32"
    {
        Ok(venial::TypeExpr {
            tokens: vec![TokenTree::Ident(ident("f64"))],
        })
    } else {
        Err(param_ty)
    }
}

pub(crate) fn maybe_rename_parameter(param_ident: Ident, next_unnamed_index: &mut i32) -> Ident {
    // Parameter will be forwarded as an argument to the instance, so we need to give `_` a name.
    let param_str = param_ident.to_string(); // a pity that Ident has no string operations.

    if param_str == "_" {
        let ident = format_ident!("__unnamed_{next_unnamed_index}");
        *next_unnamed_index += 1;
        ident
    } else if let Some(remain) = param_str.strip_prefix('_') {
        // If parameters are currently unused, still use the actual name, as "used-ness" is an implementation detail.
        // This could technically collide with another parameter of the same name (without "_"), but that's very unlikely and not
        // something we really need to support.
        // Note that the case of a single "_" is handled above.
        safe_ident(remain)
    } else {
        param_ident
    }
}

fn make_method_flags(
    method_type: ReceiverType,
    is_script_virtual: bool,
) -> Result<TokenStream, String> {
    let flags = quote! { ::godot::global::MethodFlags };

    let base_flags = match method_type {
        ReceiverType::Ref => {
            quote! { #flags::NORMAL | #flags::CONST }
        }
        // Conservatively assume Gd<Self> receivers to mutate the object, since user can call bind_mut().
        ReceiverType::Mut | ReceiverType::GdSelf => {
            quote! { #flags::NORMAL }
        }
        ReceiverType::Static => {
            if is_script_virtual {
                return Err(
                    "#[func(virtual)] is not allowed for associated (static) functions".to_string(),
                );
            }
            quote! { #flags::NORMAL | #flags::STATIC }
        }
    };

    let flags = if is_script_virtual {
        quote! { #base_flags | #flags::VIRTUAL }
    } else {
        base_flags
    };

    Ok(flags)
}

/// Generate code for a C FFI function that performs a varcall.
fn make_varcall_fn(
    call_ctx: &TokenStream,
    wrapped_method: &TokenStream,
    func: &FuncDefinition,
) -> TokenStream {
    let std_param_names = func
        .signature_info
        .std_params
        .iter()
        .map(|p| &p.ident)
        .collect::<Vec<_>>();
    let default_param_names = func
        .signature_info
        .default_params
        .iter()
        .map(|(p, _)| &p.ident)
        .collect::<Vec<_>>();
    let default_param_values = func
        .signature_info
        .default_params
        .iter()
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    let default_param_types = func
        .signature_info
        .default_params
        .iter()
        .map(|(p, _)| &p.ty);

    let base_offset = func.signature_info.std_params.len();
    let default_param_offsets = (0..func.signature_info.default_params.len())
        .map(|i| base_offset + i)
        .collect::<Vec<_>>();

    // TODO reduce amount of code generated, by delegating work to a library function. Could even be one that produces this function pointer.
    quote! {
        unsafe extern "C" fn varcall_fn(
            _method_data: *mut std::ffi::c_void,
            instance_ptr: sys::GDExtensionClassInstancePtr,
            args_ptr: *const sys::GDExtensionConstVariantPtr,
            arg_count: sys::GDExtensionInt,
            ret: sys::GDExtensionVariantPtr,
            err: *mut sys::GDExtensionCallError,
        ) {
            let call_ctx = #call_ctx;

            ::godot::private::handle_varcall_panic(
                &call_ctx,
                &mut *err,
                || {
                    let arg_count = arg_count as usize;
                    if arg_count < CallStdParams::LEN {
                        return Err(::godot::meta::error::CallError::failed_param_count(&call_ctx, arg_count, CallStdParams::LEN));
                    }

                    if arg_count > CallParams::LEN {
                        return Err(::godot::meta::error::CallError::failed_param_count(&call_ctx, arg_count, CallParams::LEN));
                    }

                    let (#(#std_param_names,)*) =
                        unsafe { CallStdParams::from_varcall_args(args_ptr, &call_ctx)? };

                    #(
                        let #default_param_names: #default_param_types = if #default_param_offsets < arg_count {
                            let arg = unsafe { *args_ptr.add(#default_param_offsets) };
                            unsafe {
                                ::godot::meta::varcall_arg::<#default_param_types>(
                                    arg,
                                    &call_ctx,
                                    #default_param_offsets as isize,
                                )?
                            }
                        } else {
                            #default_param_values
                        };
                    )*;

                    let args = (
                        #(#std_param_names,)*
                        #(#default_param_names,)*
                    );

                    let func = #wrapped_method;
                    let rust_result = unsafe { func(instance_ptr, args) };
                    unsafe { ::godot::meta::varcall_return::<CallRet>(rust_result, ret, err) };
                    Ok(())
                }
            );
        }
    }
}

/// Generate code for a C FFI function that performs a ptrcall.
fn make_ptrcall_fn(call_ctx: &TokenStream, wrapped_method: &TokenStream) -> TokenStream {
    let invocation = make_ptrcall_invocation(wrapped_method, false);

    quote! {
        unsafe extern "C" fn ptrcall_fn(
            _method_data: *mut std::ffi::c_void,
            instance_ptr: sys::GDExtensionClassInstancePtr,
            args_ptr: *const sys::GDExtensionConstTypePtr,
            ret: sys::GDExtensionTypePtr,
        ) {
            let call_ctx = #call_ctx;
            let _success = ::godot::private::handle_panic(
                || format!("{call_ctx}"),
                || #invocation
            );

            // if success.is_err() {
            //     // TODO set return value to T::default()?
            // }
        }
    }
}

/// Generate code for a `ptrcall` call expression.
fn make_ptrcall_invocation(wrapped_method: &TokenStream, is_virtual: bool) -> TokenStream {
    let ptrcall_type = if is_virtual {
        quote! { sys::PtrcallType::Virtual }
    } else {
        quote! { sys::PtrcallType::Standard }
    };

    quote! {
        ::godot::meta::Signature::<CallParams, CallRet>::in_ptrcall(
            instance_ptr,
            &call_ctx,
            args_ptr,
            ret,
            #wrapped_method,
            #ptrcall_type,
        )
    }
}

fn make_call_context(class_name_str: &str, method_name_str: &str) -> TokenStream {
    quote! {
        ::godot::meta::CallContext::func(#class_name_str, #method_name_str)
    }
}
