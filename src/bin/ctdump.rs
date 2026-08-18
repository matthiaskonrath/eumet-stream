//! Read the cloud-type field and its palette, to validate chunked+deflate decoding.

use eumet_stream::hdf5::{H5File, Layout};

fn main() {
    let path = std::env::args().nth(1).expect("usage: ctdump <file.nc>");
    let f = H5File::open(&path).expect("open");
    let links = f.links(f.root_addr).expect("links");

    // ct_conditions packs geography and illumination into bit fields.
    if let Some((_, addr)) = links.iter().find(|(n, _)| n == "ct_conditions") {
        if let Ok(ds) = f.dataset(*addr) {
            if let Ok(raw) = f.read_raw(&ds) {
                let n = ds.elem_count();
                let vals = eumet_stream::hdf5::read_ints(&raw, &ds.dtype, n);
                let (mut land, mut sea, mut coast, mut space) = (0, 0, 0, 0);
                let (mut day, mut night, mut twilight) = (0, 0, 0);
                for v in &vals {
                    match v & 48 {
                        16 => land += 1,
                        32 => sea += 1,
                        48 => coast += 1,
                        _ => space += 1,
                    }
                    match v & 6 {
                        2 => night += 1,
                        4 => day += 1,
                        6 => twilight += 1,
                        _ => {}
                    }
                }
                println!(
                    "ct_conditions: dims={:?} type_size={}",
                    ds.dims, ds.dtype.size
                );
                let pct = |x: usize| 100.0 * x as f64 / n as f64;
                println!(
                    "  surface : land {:.1}%  sea {:.1}%  coast {:.1}%  none {:.1}%",
                    pct(land),
                    pct(sea),
                    pct(coast),
                    pct(space)
                );
                println!(
                    "  light   : day {:.1}%  night {:.1}%  twilight {:.1}%",
                    pct(day),
                    pct(night),
                    pct(twilight)
                );
            }
        }
    }

    for want in ["ct", "ct_pal"] {
        let (_, addr) = links.iter().find(|(n, _)| n == want).expect("member");
        let ds = match f.dataset(*addr) {
            Ok(d) => d,
            Err(e) => {
                println!("{want}: dataset failed: {e}");
                continue;
            }
        };
        let layout = match &ds.layout {
            Layout::Compact { len, .. } => format!("compact({len})"),
            Layout::Contiguous { size, .. } => format!("contiguous({size})"),
            Layout::Chunked { dims, .. } => format!("chunked{dims:?}"),
        };
        println!(
            "\n{want}: dims={:?} class={:?} size={} {} filters={:?}",
            ds.dims,
            ds.dtype.class,
            ds.dtype.size,
            layout,
            ds.filters.iter().map(|f| f.id).collect::<Vec<_>>()
        );

        let raw = match f.read_raw(&ds) {
            Ok(r) => r,
            Err(e) => {
                println!("  read failed: {e}");
                continue;
            }
        };
        println!("  decoded {} bytes", raw.len());

        if want == "ct" {
            let mut hist = [0usize; 256];
            for &b in &raw {
                hist[b as usize] += 1;
            }
            let total: usize = hist.iter().sum();
            println!("  value histogram (non-zero classes):");
            for (v, &n) in hist.iter().enumerate() {
                if n > 0 {
                    println!(
                        "    {v:>3} : {n:>9}  ({:5.2}%)",
                        100.0 * n as f64 / total as f64
                    );
                }
            }
        } else {
            println!("  first 18 palette entries:");
            for i in 0..18.min(ds.dims[0] as usize) {
                let o = i * 3;
                if o + 2 < raw.len() {
                    println!(
                        "    {i:>3} : ({:3},{:3},{:3})",
                        raw[o],
                        raw[o + 1],
                        raw[o + 2]
                    );
                }
            }
        }
    }
}
