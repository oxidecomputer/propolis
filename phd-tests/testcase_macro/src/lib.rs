// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use proc_macro::TokenStream;
use proc_macro_error::{abort, proc_macro_error};
use quote::quote;
use syn::{parse_macro_input, spanned::Spanned, ItemFn};

/// The macro for labeling PHD testcases.
///
/// PHD testcase functions have the signature `fn test(ctx:
/// phd_testcase::TestContext)`. The macro inserts the function body into a
/// wrapper function that returns a `phd_testcase::TestOutcome` and creates an
/// entry in the test case inventory that allows the PHD runner to enumerate the
/// test.
#[proc_macro_error]
#[proc_macro_attribute]
pub fn phd_testcase(_attrib: TokenStream, input: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(input as ItemFn);

    // Build the inventory record for this test. The `module_path!()` in the
    // generated code allows the test case to report the fully-qualified path to
    // itself regardless of where it's located.
    let fn_ident = item_fn.sig.ident.clone();
    let fn_name = fn_ident.to_string();
    let submit: proc_macro2::TokenStream = quote! {
        phd_testcase::inventory_submit! {
            phd_testcase::TestCase::new(
                module_path!(),
                #fn_name,
                phd_testcase::TestFunction { f: |ctx| Box::pin(#fn_ident(ctx)) }
            )
        }
    };

    if item_fn.sig.asyncness.is_none() {
        abort!(item_fn.sig.span(), "PHD test cases must be async");
    }

    // Rebuild the test body into an immediately-executed function that returns
    // an `anyhow::Result`. This allows tests to use the `?` operator and to
    // `return Ok(())` to allow a test to pass early.
    let fn_vis = item_fn.vis.clone();
    let fn_sig = item_fn.sig.clone();
    let fn_block = item_fn.block.stmts;

    let remade_fn = quote! {
        #fn_vis #fn_sig -> TestOutcome {
            use tracing::Instrument;
            let res: phd_testcase::Result<()> = async {
                #(#fn_block)*
                Ok(())
            }.instrument(tracing::info_span!("test", path = %concat!(module_path!(), "::", #fn_name))).await;
            match res {
                Ok(()) => phd_testcase::TestOutcome::Passed,
                Err(e) => {
                    // Treat the test as skipped if the error downcasts to the
                    // phd_testcase "skipped" error type; otherwise, treat
                    // errors as failures.
                    if let Some(skipped) = e.downcast_ref::<phd_testcase::TestSkippedError>() {
                        let phd_testcase::TestSkippedError::TestSkipped(msg) = skipped;
                        phd_testcase::TestOutcome::Skipped(msg.clone())
                    } else {
                        let msg = format!("{e:?}");
                        phd_testcase::TestOutcome::Failed(Some(msg))
                    }
                }
            }
        }
    };

    quote! {
        #remade_fn

        #submit
    }
    .into()
}

/// Marks a test as skipped. The macro can take as an argument any expression
/// that has a `to_string` method.
#[proc_macro]
pub fn phd_skip(args: TokenStream) -> TokenStream {
    let args = if args.is_empty() {
        None
    } else {
        let lit = parse_macro_input!(args as syn::Lit);
        Some(lit)
    };

    let err_inner = match args {
        None => quote! { None },
        Some(_) => {
            let stringified = quote! { (#args).to_string() };
            quote! { Some(#stringified) }
        }
    };

    // Emit an early return that returns a `phd_testcase::TestSkippedError`.
    // The `phd_testcase` macro will try to downcast any errors returned from
    // the test body into this specific error type and will mark the test as
    // skipped if the downcast succeeds.
    quote! { return Err(phd_testcase::TestSkippedError::TestSkipped(#err_inner).into()); }
        .into()
}
