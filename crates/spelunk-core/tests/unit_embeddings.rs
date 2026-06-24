//! Unit tests for embedding helpers (vec_to_blob / blob_to_vec roundtrip,
//! int8 quantisation for sqlite-vec `int8[N]` storage).

use spelunk_core::embeddings::{
    EMBEDDING_DIM, INT8_SCALE, blob_to_vec, vec_to_blob, vec_to_int8_blob,
};

#[test]
fn roundtrip_empty_vec() {
    let v: Vec<f32> = vec![];
    assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
}

#[test]
fn roundtrip_single_value() {
    let v = vec![1.0_f32];
    assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
}

#[test]
fn roundtrip_multi_value() {
    let v: Vec<f32> = vec![0.0, 1.0, -1.0, f32::MAX, f32::MIN_POSITIVE];
    let result = blob_to_vec(&vec_to_blob(&v));
    for (a, b) in v.iter().zip(result.iter()) {
        assert_eq!(a.to_bits(), b.to_bits(), "bit-exact roundtrip failed");
    }
}

#[test]
fn blob_length_is_four_bytes_per_float() {
    let v: Vec<f32> = vec![1.0, 2.0, 3.0];
    assert_eq!(vec_to_blob(&v).len(), 12);
}

#[test]
fn blob_to_vec_ignores_trailing_incomplete_chunk() {
    // 13 bytes → 3 complete f32s (12 bytes) + 1 leftover byte (ignored)
    let mut blob = vec_to_blob(&[1.0_f32, 2.0, 3.0]);
    blob.push(0xFF);
    let result = blob_to_vec(&blob);
    assert_eq!(result.len(), 3);
}

// ── int8 quantisation (PR #441 / spelunk-oss#9) ────────────────────────────────

/// Read an int8 blob back as `i8` values for assertions.
fn as_i8(blob: &[u8]) -> Vec<i8> {
    blob.iter().map(|&b| b as i8).collect()
}

#[test]
fn int8_blob_is_one_byte_per_component() {
    let v = vec![0.0_f32; EMBEDDING_DIM];
    assert_eq!(vec_to_int8_blob(&v).len(), EMBEDDING_DIM);
    // 4× smaller than the f32 blob — the headline storage win of #441.
    assert_eq!(vec_to_int8_blob(&v).len() * 4, vec_to_blob(&v).len());
}

#[test]
fn int8_quantises_with_round_half_away_from_zero_times_127() {
    // round(x * 127): 1.0→127, -1.0→-127, 0→0, 0.5→64 (63.5 rounds away), -0.5→-64.
    let v = vec![1.0_f32, -1.0, 0.0, 0.5, -0.5];
    assert_eq!(as_i8(&vec_to_int8_blob(&v)), vec![127, -127, 0, 64, -64]);
}

#[test]
fn int8_clamps_out_of_unit_range_no_wraparound() {
    // Unit vectors stay in [-1, 1], but components just over the edge (rounding
    // slack) must clamp to ±127, never wrap to a large-magnitude opposite sign.
    let v = vec![2.0_f32, -3.0, 1.0001, -1.0001];
    let q = as_i8(&vec_to_int8_blob(&v));
    assert_eq!(q, vec![127, -127, 127, -127]);
    assert!(q.iter().all(|&x| (-127..=127).contains(&x)));
}

#[test]
fn int8_scale_is_the_quantisation_factor() {
    // Distances from sqlite-vec `int8` L2 are this factor larger than f32; the
    // read path divides by it. Drift here silently mis-scales every distance.
    assert_eq!(INT8_SCALE, 127.0);
}
