// Copyright 2018 Guillaume Pinot (@TeXitoi) <texitoi@texitoi.eu>,
// Kevin Knapp (@kbknapp) <kbknapp@gmail.com>, and
// Andrew Hobden (@hoverbear) <andrew@hoverbear.org>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// This work was derived from Structopt (https://github.com/TeXitoi/structopt)
// commit#ea76fa1b1b273e65e3b0b1046643715b49bec51f which is licensed under the
// MIT/Apache 2.0 license.

use std::env;

use proc_macro2::TokenStream;
use proc_macro_error::{abort, abort_call_site};
use quote::{quote, quote_spanned};
use syn::{punctuated::Punctuated, spanned::Spanned, Token};

use super::{
    dummies, spanned::Sp, ty::Ty, Attrs, GenOutput, Kind, Name, ParserKind, DEFAULT_CASING,
    DEFAULT_ENV_CASING,
};
use crate::derives::ty::sub_type;

pub fn derive_into_app(input: &syn::DeriveInput) -> proc_macro2::TokenStream {
    use syn::Data::*;

    dummies::into_app(&input.ident);

    match input.data {
        Struct(syn::DataStruct {
            fields: syn::Fields::Named(ref fields),
            ..
        }) => gen_for_struct(&input.ident, &fields.named, &input.attrs).0,
        Enum(_) => gen_for_enum(&input.ident),
        _ => abort_call_site!("#[derive(IntoApp)] only supports non-tuple structs and enums"),
    }
}

pub fn gen_for_struct(
    struct_name: &syn::Ident,
    fields: &Punctuated<syn::Field, Token![,]>,
    attrs: &[syn::Attribute],
) -> GenOutput {
    let (into_app, attrs) = gen_into_app_fn(attrs);
    let augment_clap = gen_augment_clap_fn(fields, &attrs);

    let tokens = quote! {
        impl ::clap::IntoApp for #struct_name {
            #into_app
            #augment_clap
        }
    };

    (tokens, attrs)
}

pub fn gen_for_enum(name: &syn::Ident) -> TokenStream {
    let app_name = env::var("CARGO_PKG_NAME").ok().unwrap_or_default();

    quote! {
        impl ::clap::IntoApp for #name {
            fn into_app<'b>() -> ::clap::App<'b> {
                let app = ::clap::App::new(#app_name)
                    .setting(::clap::AppSettings::SubcommandRequiredElseHelp);
                <#name as ::clap::IntoApp>::augment_clap(app)
            }

            fn augment_clap<'b>(app: ::clap::App<'b>) -> ::clap::App<'b> {
                <#name as ::clap::Subcommand>::augment_subcommands(app)
            }
        }
    }
}

fn gen_into_app_fn(attrs: &[syn::Attribute]) -> GenOutput {
    let app_name = env::var("CARGO_PKG_NAME").ok().unwrap_or_default();

    let attrs = Attrs::from_struct(
        proc_macro2::Span::call_site(),
        attrs,
        Name::Assigned(quote!(#app_name)),
        Sp::call_site(DEFAULT_CASING),
        Sp::call_site(DEFAULT_ENV_CASING),
    );
    let name = attrs.cased_name();

    let tokens = quote! {
        fn into_app<'b>() -> ::clap::App<'b> {
            Self::augment_clap(::clap::App::new(#name))
        }
    };

    (tokens, attrs)
}

fn gen_augment_clap_fn(
    fields: &Punctuated<syn::Field, Token![,]>,
    parent_attribute: &Attrs,
) -> proc_macro2::TokenStream {
    let app_var = syn::Ident::new("app", proc_macro2::Span::call_site());
    let augmentation = gen_app_augmentation(fields, &app_var, parent_attribute);
    quote! {
        fn augment_clap<'b>(#app_var: ::clap::App<'b>) -> ::clap::App<'b> {
            #augmentation
        }
    }
}

/// Generate a block of code to add arguments/subcommands corresponding to
/// the `fields` to an app.
pub fn gen_app_augmentation(
    fields: &Punctuated<syn::Field, Token![,]>,
    app_var: &syn::Ident,
    parent_attribute: &Attrs,
) -> proc_macro2::TokenStream {
    let mut subcmds = fields.iter().filter_map(|field| {
        let attrs = Attrs::from_field(
            &field,
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );
        let kind = attrs.kind();
        if let Kind::Subcommand(ty) = &*kind {
            let subcmd_type = match (**ty, sub_type(&field.ty)) {
                (Ty::Option, Some(sub_type)) => sub_type,
                _ => &field.ty,
            };
            let required = if **ty == Ty::Option {
                quote!()
            } else {
                quote_spanned! { kind.span()=>
                    let #app_var = #app_var.setting(
                        ::clap::AppSettings::SubcommandRequiredElseHelp
                    );
                }
            };

            let span = field.span();
            let ts = quote! {
                let #app_var = <#subcmd_type as ::clap::Subcommand>::augment_subcommands( #app_var );
                #required
            };
            Some((span, ts))
        } else {
            None
        }
    });
    let subcmd = subcmds.next().map(|(_, ts)| ts);
    if let Some((span, _)) = subcmds.next() {
        abort!(
            span,
            "multiple subcommand sets are not allowed, that's the second"
        );
    }

    let args = fields.iter().filter_map(|field| {
        let attrs = Attrs::from_field(
            field,
            parent_attribute.casing(),
            parent_attribute.env_casing(),
        );
        let kind = attrs.kind();
        match &*kind {
            Kind::Subcommand(_) | Kind::Skip(_) => None,
            Kind::Flatten => {
                let ty = &field.ty;
                Some(quote_spanned! { kind.span()=>
                    let #app_var = <#ty as ::clap::IntoApp>::augment_clap(#app_var);
                })
            }
            Kind::Arg(ty) => {
                let convert_type = match **ty {
                    Ty::Vec | Ty::Option => sub_type(&field.ty).unwrap_or(&field.ty),
                    Ty::OptionOption | Ty::OptionVec => {
                        sub_type(&field.ty).and_then(sub_type).unwrap_or(&field.ty)
                    }
                    _ => &field.ty,
                };

                let occurrences = *attrs.parser().kind == ParserKind::FromOccurrences;
                let flag = *attrs.parser().kind == ParserKind::FromFlag;

                let parser = attrs.parser();
                let func = &parser.func;
                let validator = match *parser.kind {
                    ParserKind::TryFromStr => quote_spanned! { func.span()=>
                        .validator(|s| {
                            #func(s.as_str())
                            .map(|_: #convert_type| ())
                            .map_err(|e| e.to_string())
                        })
                    },
                    ParserKind::TryFromOsStr => quote_spanned! { func.span()=>
                        .validator_os(|s| #func(&s).map(|_: #convert_type| ()))
                    },
                    _ => quote!(),
                };

                let modifier = match **ty {
                    Ty::Bool => quote!(),

                    Ty::Option => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        #validator
                    },

                    Ty::OptionOption => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(false)
                        .min_values(0)
                        .max_values(1)
                        #validator
                    },

                    Ty::OptionVec => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(true)
                        .min_values(0)
                        #validator
                    },

                    Ty::Vec => quote_spanned! { ty.span()=>
                        .takes_value(true)
                        .multiple(true)
                        #validator
                    },

                    Ty::Other if occurrences => quote_spanned! { ty.span()=>
                        .multiple_occurrences(true)
                    },

                    Ty::Other if flag => quote_spanned! { ty.span()=>
                        .takes_value(false)
                        .multiple(false)
                    },

                    Ty::Other => {
                        let required = !attrs.has_method("default_value");
                        quote_spanned! { ty.span()=>
                            .takes_value(true)
                            .required(#required)
                            #validator
                        }
                    }
                };

                let name = attrs.cased_name();
                let methods = attrs.field_methods();

                Some(quote_spanned! { field.span()=>
                    let #app_var = #app_var.arg(
                        ::clap::Arg::with_name(#name)
                            #modifier
                            #methods
                    );
                })
            }
        }
    });

    let app_methods = parent_attribute.top_level_methods();
    let version = parent_attribute.version();
    quote! {{
        let #app_var = #app_var#app_methods;
        #( #args )*
        #subcmd
        #app_var#version
    }}
}
