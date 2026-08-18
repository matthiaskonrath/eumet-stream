//! Structural dump of an HDF5/netCDF-4 file, used to validate the reader
//! against real EUMETCast products.

use eumet_stream::hdf5::{AttrValue, H5File};

fn show(v: &AttrValue) -> String {
    match v {
        AttrValue::Text(s) => {
            let s = s.replace('\n', " ");
            if s.len() > 150 {
                format!("\"{}...\"", &s[..150])
            } else {
                format!("\"{s}\"")
            }
        }
        AttrValue::Ints(v) => format!("{:?}", &v[..v.len().min(12)]),
        AttrValue::Floats(v) => format!("{:?}", &v[..v.len().min(12)]),
        AttrValue::Raw(b) => format!("<{} raw bytes>", b.len()),
    }
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: h5dump <file.nc>");
            std::process::exit(2);
        }
    };

    let f = match H5File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    println!("file : {path}");
    println!("size : {} bytes\n", f.data.len());

    let links = match f.links(f.root_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to enumerate root links: {e}");
            Vec::new()
        }
    };
    println!("=== root members ({}) ===", links.len());
    for (name, addr) in &links {
        println!("  {name:<24} @ {addr}");
    }

    println!("\n=== global attributes ===");
    match f.attributes(f.root_addr) {
        Ok(a) => {
            let mut keys: Vec<_> = a.keys().cloned().collect();
            keys.sort();
            for k in keys {
                println!("  {:<28} = {}", k, show(&a[&k]));
            }
        }
        Err(e) => println!("  failed: {e}"),
    }

    for (name, addr) in &links {
        println!("\n=== {name} ===");
        match f.attributes(*addr) {
            Ok(a) => {
                let mut keys: Vec<_> = a.keys().cloned().collect();
                keys.sort();
                for k in keys {
                    println!("  {:<26} = {}", k, show(&a[&k]));
                }
            }
            Err(e) => println!("  attrs failed: {e}"),
        }
    }
}
