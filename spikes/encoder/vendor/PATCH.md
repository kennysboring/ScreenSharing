# Vendored cros-libva 0.0.12

`cros-codecs` 0.0.6 requires exactly `cros-libva` 0.0.12. That version builds its bindings with
bindgen from the system libva headers, and libva 2.23+ added two fields to
`VAEncPictureParameterBufferVP9` (`seg_id_block_size`, `va_reserved8`). The hand-written
initializer in `src/buffer/vp9.rs` does not set them, so the crate does not compile against
libva 2.24.

Change: those two fields are set to zero in `EncPictureParameterBufferVP9::new`. Only VP9
encode code is touched; the H.264 path used by the spike is unchanged.

The fix assumes libva >= 2.23 (it would not compile on older headers). Upstream fixed it in
cros-libva commit 59384456ac (2026-09-01, version 0.0.13), but with a changed signature that
cros-codecs 0.0.6 cannot call, so it cannot be used directly.

License: BSD-3-Clause, see `cros-libva/LICENSE`. The published crate does not ship that file,
so it was copied from the upstream repository (github.com/chromeos/cros-libva).
