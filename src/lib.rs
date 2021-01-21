#[macro_use]
extern crate log;
#[macro_use]
extern crate may;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate serde_derive;

extern crate hashbrown;
extern crate may_waiter;
extern crate rcu_cell;
extern crate nrsc_object_base;
extern crate nrsc_wallet_base;
extern crate serde;
extern crate smallvec;
extern crate tungstenite;
extern crate url;

pub use nrsc_wallet_base::base64;
pub use nrsc_wallet_base::rand;
pub use nrsc_wallet_base::secp256k1;
pub use nrsc_wallet_base::sha2;

#[macro_export]
macro_rules! t {
    ($e:expr) => {
        match $e {
            Ok(val) => val,
            Err(err) => {
                error!("call = {:?} err = {}", stringify!($e), err);
            }
        }
    };
}

#[macro_export]
macro_rules! t_c {
    ($e:expr) => {
        match $e {
            Ok(val) => val,
            Err(err) => {
                error!("call = {:?} err = {}", stringify!($e), err);
                continue;
            }
        }
    };
}

// this is a special go macro that can return Result and print the error and backtrace
#[macro_export]
macro_rules! try_go {
    ($func:expr) => {{
        fn _go_check<F, E>(f: F) -> F
        where
            F: FnOnce() -> ::std::result::Result<(), E> + Send + 'static,
            E: Send + 'static,
        {
            f
        }
        let f = _go_check($func);
        go!(move || if let Err(e) = f() {
            error!("coroutine error: {}", e);
        })
    }};

    // for builder/scope spawn
    ($builder:expr, $func:expr) => {{
        fn _go_check<F, E>(f: F) -> F
        where
            F: FnOnce() -> ::std::result::Result<(), E> + Send,
            E: Send,
        {
            f
        }
        let f = _go_check($func);
        go!($builder, move || if let Err(e) = f() {
            error!("coroutine error: {}", e);
        })
    }};
}

#[macro_use]
pub mod utils;

pub mod business;
pub mod cache;
pub mod catchup;
pub mod composer;
pub mod config;
pub mod error;
pub mod explore;
pub mod finalization;
pub mod joint;
pub mod kv_store;
pub mod light;
pub mod main_chain;
pub mod my_witness;
pub mod network;
pub mod notify_watcher;
pub mod paid_witnessing;
pub mod signature;
pub mod spec;
pub mod statistics;
pub mod time;
pub mod validation;
pub mod wallet_info;
pub mod witness_proof;
