/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::ParseResult;
use crate::class::{transform_gd_impl, transform_inherent_impl, transform_trait_impl};
use crate::util::{KvParser, bail, venial_parse_meta};

fn parse_inherent_impl_attr(meta: TokenStream) -> Result<super::InherentImplAttr, venial::Error> {
    let item = venial_parse_meta(&meta, format_ident!("godot_api"), &quote! { fn func(); })?;
    let mut attr = KvParser::parse_required(item.attributes(), "godot_api", &meta)?;
    let secondary = attr.handle_alone("secondary")?;
    let no_typed_signals = attr.handle_alone("no_typed_signals")?;
    let no_typed_rpcs = attr.handle_alone("no_typed_rpcs")?;
    attr.finish()?;

    if no_typed_signals && secondary {
        return bail!(
            meta,
            "#[godot_api]: keys `secondary` and `no_typed_signals` are mutually exclusive; secondary blocks allow no signals anyway"
        )?;
    }
    if no_typed_rpcs && secondary {
        return bail!(
            meta,
            "#[godot_api]: keys `secondary` and `no_typed_rpcs` are mutually exclusive; secondary blocks don't allow rpcs anyway"
        )?;
    }

    Ok(super::InherentImplAttr {
        secondary,
        no_typed_signals,
        no_typed_rpcs,
    })
}

pub fn attribute_godot_api(
    meta: TokenStream,
    input_decl: venial::Item,
) -> ParseResult<TokenStream> {
    let decl = match input_decl {
        venial::Item::Impl(decl) => decl,
        _ => bail!(
            input_decl,
            "#[godot_api] can only be applied on impl blocks",
        )?,
    };

    if decl.impl_generic_params.is_some() {
        bail!(
            &decl,
            "#[godot_api] does not support lifetimes or generic parameters",
        )?;
    }

    let Some(self_path) = decl.self_ty.as_path() else {
        return bail!(decl, "invalid Self type for #[godot_api] impl");
    };

    if let Some(class_name) = extract_gd_class_name(&self_path)? {
        if !meta.is_empty() {
            return bail!(
                meta,
                "#[godot_api] on `Gd<UserClass>` impl blocks does not support parameters; these blocks are implicitly secondary"
            );
        }

        return transform_gd_impl(decl, class_name);
    }

    if decl.trait_ty.is_some() {
        // 'meta' contains the parameters to the macro, that is, for `#[godot_api(a, b, x=y)]`, anything inside the braces.
        // We currently don't accept any parameters for a trait `impl`, so show an error to the user if they added something there.
        if meta.to_string() != "" {
            return bail!(
                meta,
                "#[godot_api] on a trait implementation currently does not support any parameters"
            );
        }
        transform_trait_impl(decl)
    } else {
        match parse_inherent_impl_attr(meta) {
            Ok(meta) => transform_inherent_impl(meta, decl, self_path),
            Err(err) => Err(err),
        }
    }
}

fn extract_gd_class_name(self_path: &venial::Path) -> ParseResult<Option<proc_macro2::Ident>> {
    let Some(gd_segment) = self_path.segments.last() else {
        return Ok(None);
    };

    if gd_segment.ident != "Gd" {
        return Ok(None);
    }

    let Some(generic_args) = gd_segment.generic_args.as_ref() else {
        return bail!(gd_segment, "expected `Gd<UserClass>`")?;
    };

    let [(venial::GenericArg::TypeOrConst { expr }, _)] = generic_args.args.inner.as_slice() else {
        return bail!(
            generic_args,
            "expected exactly one user class in `Gd<UserClass>`"
        )?;
    };

    let Some(class_segment) = crate::util::extract_typename(expr) else {
        return bail!(expr, "expected a user class path in `Gd<UserClass>`")?;
    };

    if class_segment.generic_args.is_some() {
        return bail!(expr, "the user class in `Gd<UserClass>` cannot be generic")?;
    }

    Ok(Some(class_segment.ident))
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::attribute_godot_api;

    fn parse_impl(tokens: proc_macro2::TokenStream) -> venial::Item {
        venial::parse_item(tokens).expect("test impl should parse")
    }

    #[test]
    fn gd_inherent_impl_expands_as_secondary_without_binding() {
        let output = attribute_godot_api(
            quote! {},
            parse_impl(quote! {
                impl Gd<Foo> {
                    #[func]
                    fn value(&self) -> i64 { 42 }

                    #[func]
                    fn owned(self) -> i64 { 42 }
                }
            }),
        )
        .expect("Gd inherent impl should expand")
        .to_string();

        assert!(output.contains("Foo :: __registration_storage"));
        assert!(output.contains("Storage :: get_gd"));
        assert!(output.contains("< Gd < Foo > > :: owned (__gdext_self"));
        assert!(!output.contains("Storage :: get ( storage )"));
        assert!(!output.contains("ImplementsGodotApi"));
    }

    #[test]
    fn gd_extension_trait_expands_with_qualified_dispatch() {
        let output = attribute_godot_api(
            quote! {},
            parse_impl(quote! {
                impl GdFooExt for Gd<Foo> {
                    #[func]
                    fn value(&self) -> i64 { 42 }
                }
            }),
        )
        .expect("Gd extension trait impl should expand")
        .to_string();

        assert!(output.contains("< Gd < Foo > as GdFooExt > :: value"));
        assert!(output.contains("Storage :: get_gd"));
    }

    #[test]
    fn gd_impl_rejects_gd_self() {
        let result = attribute_godot_api(
            quote! {},
            parse_impl(quote! {
                impl GdFooExt for Gd<Foo> {
                    #[func(gd_self)]
                    fn value(this: Gd<Foo>) -> i64 { 42 }
                }
            }),
        );

        let error = result
            .expect_err("gd_self should be rejected")
            .to_compile_error();
        assert!(
            error
                .to_string()
                .contains("gd_self)] is not allowed in `Gd<UserClass>` impl blocks")
        );
    }
}
