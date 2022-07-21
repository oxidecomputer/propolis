use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

#[proc_macro_attribute]
pub fn phd_testcase(_attrib: TokenStream, input: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(input as ItemFn);

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

    // Deconstruct the function into component parts that we can reassemble.
    let fn_vis = item_fn.vis.clone();
    let fn_sig = item_fn.sig.clone();
    let fn_block = item_fn.block.clone();
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
