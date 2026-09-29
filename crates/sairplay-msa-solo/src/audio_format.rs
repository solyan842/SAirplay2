//! Native AP2 audioFormat bit selection from pinned MSA source.
pub const ALAC_44100_16_2:u64=1u64<<18;
pub const ALAC_44100_24_2:u64=1u64<<19;
pub const ALAC_48000_16_2:u64=1u64<<20;
pub const ALAC_48000_24_2:u64=1u64<<21;

pub fn audio_format_code(sample_rate:u32, bit_depth:u16)->u64 {
    match (sample_rate>=48_000,bit_depth>16) {
        (true,true)=>ALAC_48000_24_2,
        (false,true)=>ALAC_44100_24_2,
        (true,false)=>ALAC_48000_16_2,
        (false,false)=>ALAC_44100_16_2,
    }
}
#[cfg(test)]
mod tests { use super::*; #[test] fn exact_msa_mapping(){assert_eq!(audio_format_code(44_100,16),ALAC_44100_16_2);assert_eq!(audio_format_code(48_000,24),ALAC_48000_24_2);} }
