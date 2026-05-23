use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, FnArg, GenericArgument, ImplItem, ImplItemFn, ItemImpl, LitStr, Meta, PathArguments,
    ReturnType, Type, parse_macro_input,
};

/// Implements `HealthCheck` and generates `into_health` for an impl block.
///
/// Supported method attributes are `#[liveness]`, `#[readiness]`,
/// `#[startup]`, and `#[health(...)]`. Each annotated method must be async,
/// take `&self`, take no other arguments, and return an
/// `axum_health::Result<axum_health::Check>`.
#[proc_macro_attribute]
pub fn health_check(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut impl_item = parse_macro_input!(item as ItemImpl);
    TokenStream::from(expand_health_check(&mut impl_item))
}

fn expand_health_check(impl_item: &mut ItemImpl) -> TokenStream2 {
    let axum_health = quote!(::axum_health);
    let self_ty = impl_item.self_ty.clone();
    let generics = impl_item.generics.clone();
    let (impl_generics, _ty_generics, where_clause) = generics.split_for_impl();
    let mut errors = Vec::new();
    let mut registrations = Vec::new();
    let mut has_annotated_check = false;

    if impl_item.trait_.is_some() {
        errors.push(
            syn::Error::new_spanned(
                &impl_item.self_ty,
                "#[health_check] must be used on an inherent impl block",
            )
            .to_compile_error(),
        );
    }

    for item in impl_item.items.iter_mut() {
        let ImplItem::Fn(method) = item else {
            continue;
        };

        let had_check_attrs = method.attrs.iter().any(is_check_attr);
        match take_check_attrs(&mut method.attrs) {
            Ok(checks) if checks.is_empty() => {}
            Ok(checks) => {
                has_annotated_check = true;
                match validate_method(method) {
                    Ok(()) => {
                        let method_ident = method.sig.ident.clone();
                        for check in checks {
                            let name = check.name.unwrap_or_else(|| method_ident.to_string());
                            let kind = check.kind.tokens(&axum_health);
                            registrations.push(quote! {
                                builder = builder.check_for([#kind], #name, {
                                    let checks = ::std::sync::Arc::clone(&checks);
                                    move || {
                                        let checks = ::std::sync::Arc::clone(&checks);
                                        async move { checks.#method_ident().await }
                                    }
                                });
                            });
                        }
                    }
                    Err(error) => errors.push(error.to_compile_error()),
                }
            }
            Err(error) => {
                has_annotated_check |= had_check_attrs;
                errors.push(error.to_compile_error());
            }
        }
    }

    if !has_annotated_check {
        errors.push(
            syn::Error::new_spanned(
                &impl_item.self_ty,
                "#[health_check] requires at least one annotated health check method",
            )
            .to_compile_error(),
        );
    }

    quote! {
        #impl_item

        #[automatically_derived]
        impl #impl_generics #axum_health::HealthCheck for #self_ty #where_clause {
            fn register(self, mut builder: #axum_health::HealthBuilder) -> #axum_health::HealthBuilder {
                let checks = ::std::sync::Arc::new(self);
                #(#registrations)*
                builder
            }
        }

        #[automatically_derived]
        impl #impl_generics #self_ty #where_clause {
            /// Converts this health check provider into a health registry.
            pub fn into_health(self) -> #axum_health::Health {
                #axum_health::Health::builder().include(self).build()
            }
        }

        #(#errors)*
    }
}

#[derive(Debug, Clone, Copy)]
enum CheckKind {
    Liveness,
    Readiness,
    Startup,
}

impl CheckKind {
    fn tokens(self, axum_health: &TokenStream2) -> TokenStream2 {
        match self {
            Self::Liveness => quote!(#axum_health::Kind::Liveness),
            Self::Readiness => quote!(#axum_health::Kind::Readiness),
            Self::Startup => quote!(#axum_health::Kind::Startup),
        }
    }
}

#[derive(Debug)]
struct CheckAttr {
    kind: CheckKind,
    name: Option<String>,
}

fn validate_method(method: &ImplItemFn) -> syn::Result<()> {
    if method.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &method.sig.ident,
            "health check methods must be async",
        ));
    }

    if method.sig.generics.lt_token.is_some() {
        return Err(syn::Error::new_spanned(
            &method.sig.generics,
            "health check methods must not have generic parameters",
        ));
    }

    let mut inputs = method.sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_none() => {}
        Some(input) => {
            return Err(syn::Error::new_spanned(
                input,
                "health check methods must take &self as the first parameter",
            ));
        }
        None => {
            return Err(syn::Error::new_spanned(
                &method.sig.ident,
                "health check methods must take &self",
            ));
        }
    }

    if let Some(input) = inputs.next() {
        return Err(syn::Error::new_spanned(
            input,
            "health check methods must not take arguments other than &self",
        ));
    }

    match &method.sig.output {
        ReturnType::Type(_, ty) if is_health_check_result(ty) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                &method.sig.output,
                "health check methods must return axum_health::Result<axum_health::Check>",
            ));
        }
    }

    Ok(())
}

fn is_health_check_result(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };

    let Some(result_segment) = type_path.path.segments.last() else {
        return false;
    };
    if result_segment.ident != "Result" {
        return false;
    }

    let PathArguments::AngleBracketed(args) = &result_segment.arguments else {
        return false;
    };

    let Some(GenericArgument::Type(ok_type)) = args.args.first() else {
        return false;
    };

    is_check_type(ok_type)
}

fn is_check_type(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };

    type_path
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Check")
}

fn is_check_attr(attr: &Attribute) -> bool {
    attr.path().is_ident("liveness")
        || attr.path().is_ident("readiness")
        || attr.path().is_ident("startup")
        || attr.path().is_ident("health")
}

fn take_check_attrs(attrs: &mut Vec<Attribute>) -> syn::Result<Vec<CheckAttr>> {
    let mut checks = Vec::new();
    let mut retained = Vec::new();

    for attr in std::mem::take(attrs) {
        if attr.path().is_ident("liveness") {
            checks.push(parse_single_kind_attr(&attr, CheckKind::Liveness)?);
        } else if attr.path().is_ident("readiness") {
            checks.push(parse_single_kind_attr(&attr, CheckKind::Readiness)?);
        } else if attr.path().is_ident("startup") {
            checks.push(parse_single_kind_attr(&attr, CheckKind::Startup)?);
        } else if attr.path().is_ident("health") {
            checks.extend(parse_health_attr(&attr)?);
        } else {
            retained.push(attr);
        }
    }

    *attrs = retained;
    Ok(checks)
}

fn parse_single_kind_attr(attr: &Attribute, kind: CheckKind) -> syn::Result<CheckAttr> {
    let mut name = None;
    if matches!(attr.meta, Meta::Path(_)) {
        return Ok(CheckAttr { kind, name });
    }

    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("name") {
            reject_duplicate_name(&name, &meta.path)?;
            let value = meta.value()?;
            name = Some(validate_name(value.parse::<LitStr>()?)?);
            Ok(())
        } else {
            Err(meta.error("expected `name = \"...\"`"))
        }
    })?;

    Ok(CheckAttr { kind, name })
}

fn parse_health_attr(attr: &Attribute) -> syn::Result<Vec<CheckAttr>> {
    let mut kinds = Vec::new();
    let mut name = None;

    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("liveness") {
            reject_duplicate_kind(&kinds, CheckKind::Liveness, &meta.path)?;
            kinds.push(CheckKind::Liveness);
            Ok(())
        } else if meta.path.is_ident("readiness") {
            reject_duplicate_kind(&kinds, CheckKind::Readiness, &meta.path)?;
            kinds.push(CheckKind::Readiness);
            Ok(())
        } else if meta.path.is_ident("startup") {
            reject_duplicate_kind(&kinds, CheckKind::Startup, &meta.path)?;
            kinds.push(CheckKind::Startup);
            Ok(())
        } else if meta.path.is_ident("name") {
            reject_duplicate_name(&name, &meta.path)?;
            let value = meta.value()?;
            name = Some(validate_name(value.parse::<LitStr>()?)?);
            Ok(())
        } else {
            Err(meta.error("expected `liveness`, `readiness`, `startup`, or `name = \"...\"`"))
        }
    })?;

    if kinds.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            "#[health(...)] requires at least one of `liveness`, `readiness`, or `startup`",
        ));
    }

    Ok(kinds
        .into_iter()
        .map(|kind| CheckAttr {
            kind,
            name: name.clone(),
        })
        .collect())
}

fn reject_duplicate_kind(
    kinds: &[CheckKind],
    kind: CheckKind,
    path: &syn::Path,
) -> syn::Result<()> {
    if kinds
        .iter()
        .any(|existing| std::mem::discriminant(existing) == std::mem::discriminant(&kind))
    {
        return Err(syn::Error::new_spanned(path, "duplicate health check kind"));
    }
    Ok(())
}

fn reject_duplicate_name(name: &Option<String>, path: &syn::Path) -> syn::Result<()> {
    if name.is_some() {
        return Err(syn::Error::new_spanned(path, "duplicate `name` argument"));
    }
    Ok(())
}

fn validate_name(value: LitStr) -> syn::Result<String> {
    let name = value.value();
    if name.is_empty() {
        return Err(syn::Error::new(
            value.span(),
            "health check names must not be empty",
        ));
    }
    Ok(name)
}
