//! Proc macro implementations.

use ::proc_macro2::{Span, TokenStream};
use ::quote::ToTokens;
use ::syn::{
    parse::{Parse, Parser},
    parse2, parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
    Attribute, Expr, ExprLit, FnArg, ItemFn, Lit, LitStr, Meta, MetaNameValue, Pat, PatIdent,
    PatType, ReturnType, Stmt, Token, Type,
};

mod kw {
    //! Custom keywords.

    use ::syn::custom_keyword;

    custom_keyword!(wrapped);
    custom_keyword!(defer_err);
}

/// Macro attributes.
#[derive(Debug)]
enum InBlockingAttr {
    /// Wrapped blocking operation.
    Wrapped {
        /// Keyword.
        wrapped: kw::wrapped,
        /// Eq token.
        eq_token: Token![=],
        /// Path to blocking operation.
        path: ::syn::Path,
    },
    /// Defer error information.
    DeferErr {
        /// Keyword.
        defer_err: kw::defer_err,
    },
}

impl ToTokens for InBlockingAttr {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            InBlockingAttr::Wrapped {
                wrapped,
                eq_token,
                path,
            } => {
                wrapped.to_tokens(tokens);
                eq_token.to_tokens(tokens);
                path.to_tokens(tokens);
            }
            InBlockingAttr::DeferErr { defer_err } => defer_err.to_tokens(tokens),
        }
    }
}

impl Parse for InBlockingAttr {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let lookahead = input.lookahead1();
        if lookahead.peek(kw::wrapped) {
            Ok(InBlockingAttr::Wrapped {
                wrapped: input.parse()?,
                eq_token: input.parse()?,
                path: input.parse()?,
            })
        } else if lookahead.peek(kw::defer_err) {
            Ok(InBlockingAttr::DeferErr {
                defer_err: input.parse()?,
            })
        } else {
            Err(lookahead.error())
        }
    }
}

/// If attribute is a doc attribute extract documentation.
fn extract_doc(attr: &Attribute) -> Option<String> {
    thread_local! {
        static DOC: ::syn::Path = parse_quote!(doc);
    }
    if !DOC.with(|doc| doc == attr.path()) {
        return None;
    }
    let Meta::NameValue(MetaNameValue {
        value: Expr::Lit(ExprLit {
            lit: Lit::Str(lit_str),
            ..
        }),
        ..
    }) = &attr.meta
    else {
        return None;
    };

    Some(lit_str.value())
}

/// Attribute to put on functions that should be ran in a blocking thread.
///
/// # Errors
/// Should the attribute be used on a non-function or if the function cannot be used.
pub fn in_blocking(attr: TokenStream, item: TokenStream) -> ::syn::Result<TokenStream> {
    let attr = Punctuated::<InBlockingAttr, Token![,]>::parse_terminated
        .parse2(attr)?
        .into_iter()
        .collect();
    let item = parse2(item)?;
    in_blocking_(attr, item)
}

/// Typed implementation for [in_blocking].
fn in_blocking_(attr: Vec<InBlockingAttr>, mut f: ItemFn) -> ::syn::Result<TokenStream> {
    thread_local! {
        static DOC: ::syn::Path = parse_quote!(doc);
        static PATH: ::syn::Path = parse_quote!(path);
    }

    let mut wrapped: Option<::syn::Path> = None;
    let mut comment = String::new();
    let mut defer_err = false;

    for attr in &attr {
        match attr {
            InBlockingAttr::Wrapped {
                wrapped: _,
                eq_token: _,
                path,
            } => wrapped = Some(path.clone()),
            InBlockingAttr::DeferErr { defer_err: _ } => defer_err = true,
        }
    }

    f.attrs = f
        .attrs
        .into_iter()
        .filter_map(|attr| {
            if let Some(doc) = extract_doc(&attr) {
                comment = doc;
                None
            } else {
                Some(attr)
            }
        })
        .collect();

    let wrapped = wrapped.ok_or_else(|| {
        ::syn::Error::new(
            Span::call_site(),
            format!(
                "no wrapped attribute specified, specified attributes {:?}",
                attr
            ),
        )
    })?;

    let mut args = Vec::<PatType>::new();
    let mut arg_names = Vec::new();
    let mut arg_docs = Vec::new();
    let mut paths = Vec::<Stmt>::new();
    for arg in &mut f.sig.inputs {
        let FnArg::Typed(arg) = arg else {
            return Err(::syn::Error::new(
                arg.span(),
                "argument should not be a receiver",
            ));
        };

        let mut is_path = false;
        let mut doc_attr = String::new();
        arg.attrs = std::mem::take(&mut arg.attrs)
            .into_iter()
            .filter_map(|attr| {
                if let Some(doc) = extract_doc(&attr) {
                    doc_attr = doc;
                    Ok(None)
                } else if PATH.with(|path| attr.path() == path) {
                    if matches!(attr.meta, Meta::Path(..)) {
                        is_path = true;
                        Ok(None)
                    } else {
                        Err(::syn::Error::new(
                            attr.span(),
                            "path attribute should not take any arguments",
                        ))
                    }
                } else {
                    Ok(Some(attr))
                }
                .transpose()
            })
            .collect::<Result<_, ::syn::Error>>()?;

        let Pat::Ident(PatIdent { ident, .. }) = arg.pat.as_ref() else {
            return Err(::syn::Error::new(
                arg.pat.span(),
                "all argument patterns should be identifiers",
            ));
        };

        if !doc_attr.is_empty() {
            arg_docs.push(format!("`{ident}`\n\n{doc_attr}"));
        }

        arg_names.push(ident.clone());

        if is_path {
            let ty = arg.ty.as_ref();
            paths.push(parse_quote!(let #ident: #ty = #ident.as_ref().into();));
            args.push(parse_quote!(#ident: impl AsRef<::std::path::Path>));
        } else {
            args.push(arg.clone());
        }
    }

    let mut wrapped = wrapped.into_token_stream().to_string();
    wrapped.retain(|c| !c.is_whitespace());

    let name = f.sig.ident.clone();

    let mut doc = format!("Run [{wrapped}] in a blocking thread.\nSee also [try_{name}][self::try_{name}].");
    let mut try_doc = format!("Try to run [{wrapped}] in a blocking thread.\nSee also [{name}][self::{name}].");

    if !comment.is_empty() {
        doc.push_str("\n\n");
        doc.push_str(&comment);

        try_doc.push_str("\n\n");
        try_doc.push_str(&comment);
    }

    if !arg_docs.is_empty() {
        doc.push_str("\n\n# Parameters");

        try_doc.push_str("\n\n# Parameters");
        for arg_doc in arg_docs {
            doc.push_str("\n\n");
            doc.push_str(&arg_doc);

            try_doc.push_str("\n\n");
            try_doc.push_str(&arg_doc);
        }
    }

    try_doc.push_str("\n\n# Errors\nIf the blocking task cannot be joined or has thrown a panic.");

    if defer_err {
        doc.push_str(&format!("\n\n# Errors\nSee [{wrapped}]."));
        try_doc.push_str(&format!("\n\nFor inner see [{wrapped}]."));
    }

    let err_doc = if defer_err {
        format!("# Errors\nSee [{wrapped}].")
    } else {
        String::new()
    };

    if !err_doc.is_empty() {
        doc.push_str("\n\n");
        doc.push_str(&err_doc);
    }

    doc.push_str("\n\n# Panics\nIf the blocking task cannot be joined or has thrown a panic.");

    let doc = LitStr::new(&doc, Span::call_site());
    let try_doc = LitStr::new(&try_doc, Span::call_site());
    let ret_ty = f.sig.output.clone();

    f.sig.ident = ::syn::Ident::new(&format!("__{}_blocking", f.sig.ident), f.sig.ident.span());
    f.attrs
        .push(parse_quote!(#[doc = "inner blocking function"]));

    let inner_name = &f.sig.ident;

    let ret_ty: Type = match ret_ty {
        ReturnType::Default => parse_quote!(()),
        ReturnType::Type(_, ty) => *ty,
    };

    let try_name = ::syn::Ident::new(&format!("try_{name}"), f.sig.ident.span());
    let try_ret_ty: Type = parse_quote!(::std::result::Result<#ret_ty, ::tokio::task::JoinError>);

    Ok(quote::quote!(
        #f
        #[doc = #doc]
        pub fn #name (#(#args),*) -> impl 'static + Send + ::std::future::Future<Output = #ret_ty> {
            #(#paths)*
            async move {
                unwrap_joined(
                    ::tokio::task::spawn_blocking(move || {
                        #inner_name(#(#arg_names),*)
                    })
                    .await
                )
            }
        }
        #[doc = #try_doc]
        pub fn #try_name (#(#args),*) -> impl 'static + Send + ::std::future::Future<Output = #try_ret_ty> {
            #(#paths)*
            ::tokio::task::spawn_blocking(move || {
                #inner_name(#(#arg_names),*)
            })
        }
    ))
}
