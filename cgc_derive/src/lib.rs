#![feature(proc_macro, proc_macro_lib)]

extern crate proc_macro;
extern crate syn;
extern crate synstructure;
#[macro_use]
extern crate quote;

use proc_macro::TokenStream;
use synstructure::BindStyle;

#[proc_macro_derive(CgcCompat)]
pub fn derive_cgccompat(input: TokenStream) -> TokenStream {
    let source = input.to_string();
    let mut ast = syn::parse_macro_input(&source).unwrap();

    let trace = synstructure::each_field(&mut ast, &BindStyle::Ref.into(), |bi| {
        let attr_cnt = bi.field.attrs.len();
        bi.field.attrs.retain(|attr| attr.name() != "unsafe_ignore_cgc");

        if bi.field.attrs.len() != attr_cnt {
            quote::Tokens::new()
        } else {
            quote!(mark(#bi);)
        }
    });

    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();
    let result = quote! {
        // Original struct
        #ast

        unsafe impl #impl_generics ::cgc::CgcCompat for #name #ty_generics #where_clause {
            #[inline] unsafe fn trace(&self) {
                #[allow(dead_code)]
                #[inline]
                unsafe fn mark<T: ::cgc::CgcCompat>(it: &T) {
                    ::cgc::CgcCompat::trace(it);
                }
                match *self { #trace }
            }
            #[inline] unsafe fn root(&self) {
                #[allow(dead_code)]
                #[inline]
                unsafe fn mark<T: ::cgc::CgcCompat>(it: &T) {
                    ::cgc::CgcCompat::root(it);
                }
                match *self { #trace }
            }
            #[inline] unsafe fn unroot(&self) {
                #[allow(dead_code)]
                #[inline]
                unsafe fn mark<T: ::cgc::CgcCompat>(it: &T) {
                    ::cgc::CgcCompat::unroot(it);
                }
                match *self { #trace }
            }
        }
    };

    // Generate the final value as a TokenStream and return it
    result.to_string().parse().unwrap()
}
