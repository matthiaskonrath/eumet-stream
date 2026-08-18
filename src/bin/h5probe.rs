//! Diagnostic walk of dense attribute storage.

use eumet_stream::hdf5::H5File;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: h5probe <file.nc> [member]");
    let member = args.next();

    let f = H5File::open(&path).expect("open");
    let addr = match member {
        None => f.root_addr,
        Some(name) => {
            f.links(f.root_addr)
                .expect("links")
                .into_iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("no member named {name}"))
                .1
        }
    };

    match f.attr_trace(addr) {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
        }
        Err(e) => println!("error: {e}"),
    }
}
