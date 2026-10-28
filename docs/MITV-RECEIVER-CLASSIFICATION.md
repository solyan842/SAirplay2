# Native-NTP compatibility classification

Branch: compat/mitv-happycast-raop-diag. User decision: classify by protocol
capabilities, not product name, hostname, model, IP or a fixed port. Devices
with the same protocol profile must not require per-device source edits.

The measured profile is: both AirPlay and RAOP are advertised, AirPlay feature
mask 0x0000001e527ffff7, and status flags 0x4. It is classified as
legacy-native-ntp. HappyCast is the measured example, not an identity condition.
The normal connection then performs its existing /info, transient HAP and RTSP
SETUP verification; classification alone is not proof that a receiver accepts it.
Unrecognized capability combinations retain the existing MSA policy.

The profile selects Native AP2, NTP, realtime type96, ALAC 16/44.1 and passes a
receiver-local ntp_session_compat flag into native control. This enables the
existing compatible session metadata and 7-ms minimum release spacing without
requiring a launcher or fixed IP/port. The flag defaults to false. The existing
endpoint launcher remains a diagnostic override and does not write global state.
This is the user's requested measured protocol compatibility exception, not a
claim that all RAOP advertisements support native AP2.

Tests cover the profile, missing discovery services, different capability/auth
flags, and normal GUI config using a different display name, hostname, address
and port 5030. RAOP-only SOtM and the native Apple/HomePod/AirPort/Naim capability
profiles remain under their existing policy. No source edit is required to add
another device with this same capability profile.

This is classification, not stability certification. Intermittent stutter and
initial volume application remain open hardware issues. No packet, clock,
volume or lifecycle fix is included in this classification change.
