use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

/// The macro for labeling PHD testcases.
///
/// PHD testcase functions have the signature `fn test(ctx:
/// phd_testcase::TestContext)`. The macro inserts the function body into a
/// wrapper function that returns a `phd_testcase::TestOutcome` and creates an
/// entry in the test case inventory that allows the PHD runner to enumerate the
/// test.
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
                phd_testcase::TestFunction { f: #fn_ident }
            )
        }
    };

    // Rebuild the test body into an immediately-executed function that returns
    // an `anyhow::Result`. This allows tests to use the `?` operator and to
    // `return Ok(())` to allow a test to pass early.
    let fn_vis = item_fn.vis.clone();
    let fn_sig = item_fn.sig.clone();
    let fn_block = item_fn.block;
    let remade_fn = quote! {
        #fn_vis #fn_sig -> TestOutcome {
            match || -> phd_testcase::Result<()> {
                #fn_block
                Ok(())
            }(){
                Ok(()) => phd_testcase::TestOutcome::Passed,
                Err(e) => {
                    let msg = format!("{}\n    error backtrace: {}",
                                      e.to_string(),
                                      e.backtrace());
                    phd_testcase::TestOutcome::Failed(Some(msg))
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
