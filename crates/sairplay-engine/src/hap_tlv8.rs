use std::collections::BTreeMap;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TlvTag {
    Method = 0x00,
    Identifier = 0x01,
    Salt = 0x02,
    PublicKey = 0x03,
    Proof = 0x04,
    EncryptedData = 0x05,
    State = 0x06,
    Error = 0x07,
    BackOff = 0x08,
    Certificate = 0x09,
    Signature = 0x0A,
    Permissions = 0x0B,
    FragmentData = 0x0C,
    FragmentLast = 0x0D,
    Name = 0x11,
    Flags = 0x13,
}

impl TlvTag {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Method,
            0x01 => Self::Identifier,
            0x02 => Self::Salt,
            0x03 => Self::PublicKey,
            0x04 => Self::Proof,
            0x05 => Self::EncryptedData,
            0x06 => Self::State,
            0x07 => Self::Error,
            0x08 => Self::BackOff,
            0x09 => Self::Certificate,
            0x0A => Self::Signature,
            0x0B => Self::Permissions,
            0x0C => Self::FragmentData,
            0x0D => Self::FragmentLast,
            0x11 => Self::Name,
            0x13 => Self::Flags,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tlv8Error {
    TruncatedHeader,
    TruncatedValue,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tlv8 {
    values: BTreeMap<u8, Vec<u8>>,
}

impl Tlv8 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tag: TlvTag, value: impl Into<Vec<u8>>) {
        self.values.insert(tag as u8, value.into());
    }

    pub fn insert_raw(&mut self, tag: u8, value: impl Into<Vec<u8>>) {
        self.values.insert(tag, value.into());
    }

    pub fn insert_u8(&mut self, tag: TlvTag, value: u8) {
        self.insert(tag, vec![value]);
    }

    pub fn get(&self, tag: TlvTag) -> Option<&[u8]> {
        self.values.get(&(tag as u8)).map(Vec::as_slice)
    }

    pub fn get_raw(&self, tag: u8) -> Option<&[u8]> {
        self.values.get(&tag).map(Vec::as_slice)
    }

    pub fn get_u8(&self, tag: TlvTag) -> Option<u8> {
        let value = self.get(tag)?;
        (value.len() == 1).then_some(value[0])
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();

        for (&tag, value) in &self.values {
            if value.is_empty() {
                out.push(tag);
                out.push(0);
                continue;
            }

            for chunk in value.chunks(255) {
                out.push(tag);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }

        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, Tlv8Error> {
        let mut pos = 0usize;
        let mut values = BTreeMap::<u8, Vec<u8>>::new();

        while pos < data.len() {
            if data.len() - pos < 2 {
                return Err(Tlv8Error::TruncatedHeader);
            }

            let tag = data[pos];
            let len = data[pos + 1] as usize;
            pos += 2;

            if data.len() - pos < len {
                return Err(Tlv8Error::TruncatedValue);
            }

            values
                .entry(tag)
                .or_default()
                .extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }

        Ok(Self { values })
    }

    pub fn state(&self) -> Option<u8> {
        self.get_u8(TlvTag::State)
    }

    pub fn error(&self) -> Option<u8> {
        self.get_u8(TlvTag::Error)
    }
}

pub const HAP_TRANSIENT_FLAG: u8 = 0x10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_m1_matches_reference_shape() {
        let mut tlv = Tlv8::new();
        tlv.insert_u8(TlvTag::State, 0x01);
        tlv.insert_u8(TlvTag::Method, 0x00);
        tlv.insert_u8(TlvTag::Flags, HAP_TRANSIENT_FLAG);

        let wire = tlv.encode();
        let decoded = Tlv8::decode(&wire).unwrap();

        assert_eq!(decoded.state(), Some(0x01));
        assert_eq!(decoded.get_u8(TlvTag::Method), Some(0x00));
        assert_eq!(decoded.get_u8(TlvTag::Flags), Some(0x10));
    }

    #[test]
    fn values_larger_than_255_are_fragmented_and_reassembled() {
        let value: Vec<u8> = (0..384).map(|i| (i & 0xff) as u8).collect();
        let mut tlv = Tlv8::new();
        tlv.insert(TlvTag::PublicKey, value.clone());

        let wire = tlv.encode();

        assert_eq!(wire[0], TlvTag::PublicKey as u8);
        assert_eq!(wire[1], 255);
        assert_eq!(wire[257], TlvTag::PublicKey as u8);
        assert_eq!(wire[258], 129);

        let decoded = Tlv8::decode(&wire).unwrap();
        assert_eq!(decoded.get(TlvTag::PublicKey), Some(value.as_slice()));
    }

    #[test]
    fn repeated_tag_fragments_are_concatenated_on_decode() {
        let wire = [
            TlvTag::PublicKey as u8, 3, 1, 2, 3,
            TlvTag::PublicKey as u8, 2, 4, 5,
        ];

        let decoded = Tlv8::decode(&wire).unwrap();
        assert_eq!(decoded.get(TlvTag::PublicKey), Some(&[1,2,3,4,5][..]));
    }

    #[test]
    fn unknown_tags_are_preserved() {
        let wire = [0xEE, 3, 9, 8, 7];
        let decoded = Tlv8::decode(&wire).unwrap();
        assert_eq!(decoded.get_raw(0xEE), Some(&[9,8,7][..]));
        assert_eq!(TlvTag::from_u8(0xEE), None);
    }

    #[test]
    fn truncated_value_is_rejected() {
        assert_eq!(
            Tlv8::decode(&[TlvTag::Salt as u8, 4, 1, 2]),
            Err(Tlv8Error::TruncatedValue)
        );
    }
}
