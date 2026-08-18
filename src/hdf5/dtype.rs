//! HDF5 datatype messages.

use super::{err, Cur, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatatypeClass {
    FixedPoint,
    Float,
    Time,
    String,
    BitField,
    Opaque,
    Compound,
    Reference,
    Enumerated,
    VariableLength,
    Array,
    Other(u8),
}

impl DatatypeClass {
    fn from_code(c: u8) -> Self {
        match c {
            0 => DatatypeClass::FixedPoint,
            1 => DatatypeClass::Float,
            2 => DatatypeClass::Time,
            3 => DatatypeClass::String,
            4 => DatatypeClass::BitField,
            5 => DatatypeClass::Opaque,
            6 => DatatypeClass::Compound,
            7 => DatatypeClass::Reference,
            8 => DatatypeClass::Enumerated,
            9 => DatatypeClass::VariableLength,
            10 => DatatypeClass::Array,
            other => DatatypeClass::Other(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Datatype {
    pub class: DatatypeClass,
    /// Size of one element, in bytes.
    pub size: u32,
    pub signed: bool,
    pub little_endian: bool,
    /// Element type of a variable-length or array type.
    pub base: Option<Box<Datatype>>,
    /// A variable-length type whose elements are characters, i.e. a string.
    pub vlen_string: bool,
}

impl Datatype {
    pub fn parse(c: &mut Cur) -> Result<Datatype> {
        let cv = c.u8()?;
        let version = cv >> 4;
        let class = DatatypeClass::from_code(cv & 0x0F);
        if version == 0 || version > 4 {
            return err(format!("unsupported datatype version {version}"));
        }
        let bf0 = c.u8()?;
        let bf1 = c.u8()?;
        let _bf2 = c.u8()?;
        let size = c.u32()?;

        let mut dt = Datatype {
            class: class.clone(),
            size,
            signed: false,
            little_endian: true,
            base: None,
            vlen_string: false,
        };

        match class {
            DatatypeClass::FixedPoint => {
                dt.little_endian = bf0 & 0x01 == 0;
                dt.signed = bf0 & 0x08 != 0;
                let _bit_offset = c.u16()?;
                let _precision = c.u16()?;
            }
            DatatypeClass::Float => {
                dt.little_endian = bf0 & 0x01 == 0;
                c.skip(2 + 2 + 1 + 1 + 1 + 1 + 4);
            }
            DatatypeClass::String | DatatypeClass::BitField | DatatypeClass::Opaque => {}
            DatatypeClass::VariableLength => {
                // bits 0-3 of the first bit-field byte select sequence vs string.
                dt.vlen_string = (bf0 & 0x0F) == 1;
                let base = Datatype::parse(c)?;
                dt.base = Some(Box::new(base));
            }
            DatatypeClass::Array => {
                let rank = c.u8()? as usize;
                c.skip(3);
                for _ in 0..rank {
                    let _dim = c.u32()?;
                }
                if version < 3 {
                    for _ in 0..rank {
                        let _perm = c.u32()?;
                    }
                }
                let base = Datatype::parse(c)?;
                dt.base = Some(Box::new(base));
            }
            DatatypeClass::Enumerated => {
                let n = (((bf1 as u16) << 8) | bf0 as u16) as usize;
                let base = Datatype::parse(c)?;
                // Member names, then member values; both are skipped.
                for _ in 0..n {
                    if version >= 3 {
                        while c.u8()? != 0 {}
                    } else {
                        let start = c.p;
                        while c.u8()? != 0 {}
                        let used = c.p - start;
                        let pad = (8 - (used % 8)) % 8;
                        c.skip(pad);
                    }
                }
                c.skip(n * base.size as usize);
                dt.base = Some(Box::new(base));
            }
            DatatypeClass::Compound => {
                // Not needed by the products we read; leave the cursor where it
                // is rather than guessing at member layout.
            }
            _ => {}
        }
        Ok(dt)
    }

    /// True when values can be read with the numeric helpers.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.class,
            DatatypeClass::FixedPoint | DatatypeClass::Float | DatatypeClass::Enumerated
        )
    }
}
