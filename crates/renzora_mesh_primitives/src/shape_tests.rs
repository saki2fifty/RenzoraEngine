use super::*;
use bevy::mesh::VertexAttributeValues;

const EPS: f32 = 1e-3;

fn positions_of(mesh: &Mesh) -> Vec<[f32; 3]> {
    match mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap() {
        VertexAttributeValues::Float32x3(v) => v.clone(),
        _ => panic!("positions must be Float32x3"),
    }
}

fn normals_of(mesh: &Mesh) -> Vec<[f32; 3]> {
    match mesh.attribute(Mesh::ATTRIBUTE_NORMAL).unwrap() {
        VertexAttributeValues::Float32x3(v) => v.clone(),
        _ => panic!("normals must be Float32x3"),
    }
}

fn index_list(mesh: &Mesh) -> Vec<u32> {
    match mesh.indices().unwrap() {
        Indices::U32(v) => v.clone(),
        Indices::U16(v) => v.iter().map(|&i| i as u32).collect(),
    }
}

fn bounds_of(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for p in positions {
        for axis in 0..3 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    (min, max)
}

fn assert_bounds(name: &str, positions: &[[f32; 3]], want_min: [f32; 3], want_max: [f32; 3]) {
    let (min, max) = bounds_of(positions);
    for axis in 0..3 {
        assert!(
            (min[axis] - want_min[axis]).abs() < EPS,
            "{name}: axis {axis} min {} != {}",
            min[axis],
            want_min[axis]
        );
        assert!(
            (max[axis] - want_max[axis]).abs() < EPS,
            "{name}: axis {axis} max {} != {}",
            max[axis],
            want_max[axis]
        );
    }
}

fn has_vertex(positions: &[[f32; 3]], target: [f32; 3]) -> bool {
    positions.iter().any(|p| {
        (p[0] - target[0]).abs() < EPS
            && (p[1] - target[1]).abs() < EPS
            && (p[2] - target[2]).abs() < EPS
    })
}

/// Every generated mesh must have matching POSITION/NORMAL/UV counts,
/// an index buffer that's a multiple of 3, no index out of bounds,
/// unit-length normals, and triangle winding that agrees with the
/// stored vertex normals (the `ensure_correct_winding` postcondition).
fn assert_mesh_well_formed(name: &str, mesh: &Mesh) {
    let positions = positions_of(mesh);
    let normals = normals_of(mesh);
    assert!(!positions.is_empty(), "{name}: mesh has no vertices");
    assert_eq!(
        positions.len(),
        normals.len(),
        "{name}: position/normal count mismatch"
    );
    let uv_count = match mesh.attribute(Mesh::ATTRIBUTE_UV_0) {
        Some(VertexAttributeValues::Float32x2(v)) => v.len(),
        _ => panic!("{name}: uvs missing or wrong type"),
    };
    assert_eq!(
        positions.len(),
        uv_count,
        "{name}: position/uv count mismatch"
    );

    let indices = index_list(mesh);
    assert!(!indices.is_empty(), "{name}: mesh has no indices");
    assert_eq!(
        indices.len() % 3,
        0,
        "{name}: index count must be a multiple of 3"
    );
    for &i in &indices {
        assert!(
            (i as usize) < positions.len(),
            "{name}: index {i} out of bounds (verts={})",
            positions.len()
        );
    }

    for n in &normals {
        let len = Vec3::from(*n).length();
        assert!((len - 1.0).abs() < EPS, "{name}: non-unit normal {n:?}");
    }

    for tri in indices.chunks(3) {
        let a = Vec3::from(positions[tri[0] as usize]);
        let b = Vec3::from(positions[tri[1] as usize]);
        let c = Vec3::from(positions[tri[2] as usize]);
        let face = (b - a).cross(c - a);
        let stored = Vec3::from(normals[tri[0] as usize]);
        assert!(
            face.dot(stored) >= -1e-6,
            "{name}: triangle {tri:?} winding opposes its vertex normal"
        );
    }
}

// ------------------------------------------------------------------
// Construction helpers — pure, deterministic
// ------------------------------------------------------------------

#[test]
fn winding_corrected_only_when_misaligned() {
    let positions = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0], // tri 0: CCW from +Z, matches normal
        [0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [1.0, 0.0, 0.0], // tri 1: CW from +Z -> must flip
    ];
    let normals = [[0.0, 0.0, 1.0]; 6];
    let mut indices = [0u32, 1, 2, 3, 4, 5];
    ensure_correct_winding(&positions, &normals, &mut indices);
    assert_eq!(indices, [0, 1, 2, 3, 5, 4]);
}

#[test]
fn add_quad_appends_four_vertices_and_two_triangles() {
    // Pre-seed one vertex so the helper has to offset indices by `base`.
    let mut positions = vec![[9.0, 9.0, 9.0]];
    let mut normals = vec![[0.0, 1.0, 0.0]];
    let mut uvs = vec![[0.0, 0.0]];
    let mut indices = Vec::new();

    let corners = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0],
    ];
    add_quad(
        &mut positions,
        &mut normals,
        &mut uvs,
        &mut indices,
        corners,
        [0.0, 1.0, 0.0],
    );

    assert_eq!(positions.len(), 5);
    assert_eq!(normals.len(), 5);
    assert_eq!(uvs.len(), 5);
    assert_eq!(&positions[1..], corners.as_slice());
    // Two triangles fanned from the first corner, offset past the seed.
    assert_eq!(indices, vec![1, 2, 3, 1, 3, 4]);
    // The supplied normal is replicated across all four corners.
    assert!(normals[1..].iter().all(|n| *n == [0.0, 1.0, 0.0]));
}

#[test]
fn add_tri_appends_one_triangle_with_custom_uvs() {
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uvs = Vec::new();
    let mut indices = Vec::new();

    let tri_uvs = [[0.25, 0.0], [1.0, 0.5], [0.0, 1.0]];
    add_tri(
        &mut positions,
        &mut normals,
        &mut uvs,
        &mut indices,
        [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        [0.0, 0.0, 1.0],
        tri_uvs,
    );

    assert_eq!(positions.len(), 3);
    assert_eq!(indices, vec![0, 1, 2]);
    assert_eq!(uvs, tri_uvs.to_vec());
}

#[test]
fn build_mesh_applies_center_offset() {
    let positions = vec![[1.0, 2.0, 3.0], [2.0, 2.0, 3.0], [1.0, 3.0, 3.0]];
    let normals = vec![[0.0, 0.0, 1.0]; 3];
    let uvs = vec![[0.0, 0.0]; 3];
    let indices = vec![0, 1, 2];

    let mesh = build_mesh(positions, normals, uvs, indices, Some([1.0, 2.0, 3.0]));

    assert_eq!(mesh.primitive_topology(), PrimitiveTopology::TriangleList);
    let shifted = positions_of(&mesh);
    assert_eq!(
        shifted,
        vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
    );
    // CCW from +Z already agrees with the +Z normals: indices untouched.
    assert_eq!(index_list(&mesh), vec![0, 1, 2]);
    assert!(mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_some());
    assert!(mesh.attribute(Mesh::ATTRIBUTE_UV_0).is_some());
}

// ------------------------------------------------------------------
// Public mesh generators — structural validation (no GPU needed)
// ------------------------------------------------------------------

#[test]
fn all_generators_produce_well_formed_meshes() {
    let shapes = [
        ("wedge", create_wedge_mesh()),
        ("stairs", create_stairs_mesh(6)),
        ("arch", create_arch_mesh(16)),
        ("half_cylinder", create_half_cylinder_mesh(16)),
        ("quarter_pipe", create_quarter_pipe_mesh(16)),
        ("corner", create_corner_mesh()),
        ("prism", create_prism_mesh()),
        ("pyramid", create_pyramid_mesh()),
        ("pipe", create_pipe_mesh(24)),
        ("ring", create_ring_mesh(24)),
        ("ramp", create_ramp_mesh()),
        ("hemisphere", create_hemisphere_mesh(16)),
        ("curved_wall", create_curved_wall_mesh(16)),
        ("doorway", create_doorway_mesh()),
        ("window_wall", create_window_wall_mesh()),
        ("l_shape", create_l_shape_mesh()),
        ("t_shape", create_t_shape_mesh()),
        ("cross_shape", create_cross_shape_mesh()),
        ("funnel", create_funnel_mesh(24)),
        ("gutter", create_gutter_mesh(16)),
        ("spiral_stairs", create_spiral_stairs_mesh(8)),
        ("pillar", create_pillar_mesh()),
    ];
    for (name, mesh) in &shapes {
        assert_mesh_well_formed(name, mesh);
    }
}

#[test]
fn parametric_counts_are_clamped_and_scale_with_resolution() {
    // stairs: clamped to >= 2 steps
    assert_eq!(
        create_stairs_mesh(0).count_vertices(),
        create_stairs_mesh(2).count_vertices()
    );
    assert!(create_stairs_mesh(6).count_vertices() > create_stairs_mesh(2).count_vertices());
    // arch: clamped to >= 8 segments
    assert_eq!(
        create_arch_mesh(2).count_vertices(),
        create_arch_mesh(8).count_vertices()
    );
    assert!(create_arch_mesh(32).count_vertices() > create_arch_mesh(8).count_vertices());
    // spiral stairs: clamped to >= 4 steps
    assert_eq!(
        create_spiral_stairs_mesh(1).count_vertices(),
        create_spiral_stairs_mesh(4).count_vertices()
    );
    assert!(
        create_spiral_stairs_mesh(16).count_vertices()
            > create_spiral_stairs_mesh(4).count_vertices()
    );
}

#[test]
fn wedge_is_centered_with_unit_slope_normal() {
    let mesh = create_wedge_mesh();
    assert_bounds("wedge", &positions_of(&mesh), [-0.5; 3], [0.5; 3]);

    // The hypotenuse quad carries the 45-degree slope normal on exactly
    // its 4 vertices.
    let slope = Vec3::new(1.0, 1.0, 0.0).normalize();
    let slope_verts = normals_of(&mesh)
        .iter()
        .filter(|n| (Vec3::from(**n) - slope).length() < 1e-4)
        .count();
    assert_eq!(slope_verts, 4, "expected exactly one slope quad");
}

#[test]
fn ramp_slope_normal_matches_low_profile() {
    let mesh = create_ramp_mesh();
    assert_bounds(
        "ramp",
        &positions_of(&mesh),
        [-1.0, -0.25, -0.5],
        [1.0, 0.25, 0.5],
    );
    // 2.0 run over 0.5 rise: the slope normal is normalize(h, w, 0).
    let slope = Vec3::new(0.5, 2.0, 0.0).normalize();
    let slope_verts = normals_of(&mesh)
        .iter()
        .filter(|n| (Vec3::from(**n) - slope).length() < 1e-4)
        .count();
    assert_eq!(slope_verts, 4, "expected exactly one slope quad");
}

#[test]
fn hemisphere_covers_only_upper_half() {
    let mesh = create_hemisphere_mesh(16);
    let positions = positions_of(&mesh);
    for p in &positions {
        assert!(p[1] >= -EPS, "vertex below the equator: {p:?}");
        let len = Vec3::from(*p).length();
        assert!(len <= 0.5 + EPS, "vertex outside the sphere radius: {p:?}");
    }
    assert!(has_vertex(&positions, [0.0, 0.5, 0.0]), "missing pole");
    // The rim must reach the full radius at the equator (Y=0).
    assert!(has_vertex(&positions, [0.5, 0.0, 0.0]), "missing rim");
}

#[test]
fn half_cylinder_occupies_positive_z_half() {
    let mesh = create_half_cylinder_mesh(16);
    let positions = positions_of(&mesh);
    for p in &positions {
        assert!(p[2] >= -EPS, "vertex behind the cut plane: {p:?}");
        let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
        assert!(r <= 0.5 + EPS, "vertex outside the radius: {p:?}");
    }
    // The flat cut face spans the full diameter at both cap heights.
    assert!(has_vertex(&positions, [0.5, -0.5, 0.0]));
    assert!(has_vertex(&positions, [-0.5, 0.5, 0.0]));
}

#[test]
fn pipe_and_ring_span_inner_and_outer_radii() {
    let cases = [
        ("pipe", create_pipe_mesh(24), 0.35, 0.5, 0.5),
        ("ring", create_ring_mesh(24), 0.3, 0.5, 0.05),
    ];
    for (name, mesh, inner, outer, half_h) in cases {
        let positions = positions_of(&mesh);
        let mut saw_inner = false;
        let mut saw_outer = false;
        for p in &positions {
            let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
            assert!(
                r >= inner - EPS && r <= outer + EPS,
                "{name}: vertex outside the wall: {p:?}"
            );
            assert!(
                p[1].abs() <= half_h + EPS,
                "{name}: vertex outside the height: {p:?}"
            );
            saw_inner |= (r - inner).abs() < EPS;
            saw_outer |= (r - outer).abs() < EPS;
        }
        assert!(saw_inner, "{name}: no vertices on the inner radius");
        assert!(saw_outer, "{name}: no vertices on the outer radius");
    }
}

#[test]
fn funnel_tapers_from_wide_top_to_narrow_bottom() {
    let mesh = create_funnel_mesh(24);
    let positions = positions_of(&mesh);
    let max_radius_at = |y: f32| -> f32 {
        positions
            .iter()
            .filter(|p| (p[1] - y).abs() < EPS)
            .map(|p| (p[0] * p[0] + p[2] * p[2]).sqrt())
            .fold(0.0_f32, f32::max)
    };
    assert!(
        (max_radius_at(0.0) - 0.2).abs() < EPS,
        "bottom radius should be 0.2"
    );
    assert!(
        (max_radius_at(1.0) - 0.5).abs() < EPS,
        "top radius should be 0.5"
    );
    let (min, max) = bounds_of(&positions);
    assert!(min[1].abs() < EPS, "funnel should start at Y=0");
    assert!((max[1] - 1.0).abs() < EPS, "funnel should end at Y=1");
}

#[test]
fn gutter_curve_hangs_below_open_top() {
    let mesh = create_gutter_mesh(16);
    let positions = positions_of(&mesh);
    for p in &positions {
        assert!(p[1] <= EPS, "vertex above the open top: {p:?}");
        let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
        assert!(r <= 0.5 + EPS, "vertex outside the trough radius: {p:?}");
    }
    // The open top spans the full diameter on both rails.
    assert!(has_vertex(&positions, [-0.5, 0.0, -0.5]));
    assert!(has_vertex(&positions, [0.5, 0.0, 0.5]));
}

#[test]
fn spiral_stairs_cover_full_rotation_and_centered_height() {
    // Steps sweep the full 360 degrees, so the outer radius (0.5) is
    // reached on all four sides, and the 2.0 height straddles Y=0.
    let mesh = create_spiral_stairs_mesh(8);
    assert_bounds(
        "spiral_stairs",
        &positions_of(&mesh),
        [-0.5, -1.0, -0.5],
        [0.5, 1.0, 0.5],
    );
}

#[test]
fn pyramid_has_apex_and_square_base() {
    let mesh = create_pyramid_mesh();
    let positions = positions_of(&mesh);
    // 1 base quad + 4 side triangles.
    assert_eq!(positions.len(), 16);
    // The apex appears once per side face, centered after the Y offset.
    let apex_count = positions
        .iter()
        .filter(|p| p[0].abs() < EPS && (p[1] - 0.5).abs() < EPS && p[2].abs() < EPS)
        .count();
    assert_eq!(apex_count, 4, "expected the apex on all 4 side faces");
    for corner in [
        [-0.5, -0.5, -0.5],
        [0.5, -0.5, -0.5],
        [0.5, -0.5, 0.5],
        [-0.5, -0.5, 0.5],
    ] {
        assert!(
            has_vertex(&positions, corner),
            "missing base corner {corner:?}"
        );
    }
}

#[test]
fn pillar_has_octagonal_rim() {
    let mesh = create_pillar_mesh();
    let positions = positions_of(&mesh);
    for p in &positions {
        let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
        assert!(r <= 0.25 + EPS, "vertex outside the column radius: {p:?}");
        assert!(p[1].abs() <= 1.0 + EPS, "vertex outside the height: {p:?}");
    }
    // Distinct positions on the top cap: 8 octagon corners + fan center.
    let mut top: Vec<[i32; 2]> = positions
        .iter()
        .filter(|p| (p[1] - 1.0).abs() < EPS)
        .map(|p| {
            [
                (p[0] * 1000.0).round() as i32,
                (p[2] * 1000.0).round() as i32,
            ]
        })
        .collect();
    top.sort_unstable();
    top.dedup();
    assert_eq!(top.len(), 9, "expected 8 octagon corners plus the center");
}

#[test]
fn doorway_opening_is_clear() {
    let mesh = create_doorway_mesh();
    let positions = positions_of(&mesh);
    assert_bounds(
        "doorway",
        &positions,
        [-0.75, -1.0, -0.05],
        [0.75, 1.0, 0.05],
    );
    // Centered: the 0.6 x 1.4 door spans X in [-0.3, 0.3], Y in [-1.0, 0.4];
    // its interior must hold no geometry.
    for p in &positions {
        let inside = p[0] > -0.3 + EPS && p[0] < 0.3 - EPS && p[1] > -1.0 + EPS && p[1] < 0.4 - EPS;
        assert!(!inside, "vertex inside the door opening: {p:?}");
    }
    // Jamb corners frame the opening at the lintel height.
    assert!(has_vertex(&positions, [-0.3, 0.4, 0.05]));
    assert!(has_vertex(&positions, [0.3, 0.4, -0.05]));
}

#[test]
fn window_opening_is_clear_at_spec_height() {
    let mesh = create_window_wall_mesh();
    let positions = positions_of(&mesh);
    assert_bounds(
        "window_wall",
        &positions,
        [-0.75, -1.0, -0.05],
        [0.75, 1.0, 0.05],
    );
    // Centered: the 0.6 x 0.5 window (center height 1.1 on the 2.0 wall)
    // spans X in [-0.3, 0.3], Y in [-0.15, 0.35].
    for p in &positions {
        let inside =
            p[0] > -0.3 + EPS && p[0] < 0.3 - EPS && p[1] > -0.15 + EPS && p[1] < 0.35 - EPS;
        assert!(!inside, "vertex inside the window opening: {p:?}");
    }
    assert!(has_vertex(&positions, [-0.3, -0.15, 0.05]), "missing sill");
    assert!(has_vertex(&positions, [0.3, 0.35, -0.05]), "missing lintel");
}

#[test]
fn curved_wall_is_quarter_arc_between_radii() {
    let mesh = create_curved_wall_mesh(16);
    let positions = positions_of(&mesh);
    for p in &positions {
        // The 90-degree arc stays in the +X/+Z quadrant.
        assert!(
            p[0] >= -EPS && p[2] >= -EPS,
            "vertex outside quadrant: {p:?}"
        );
        assert!(
            p[1] >= -EPS && p[1] <= 1.0 + EPS,
            "vertex outside height: {p:?}"
        );
        let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
        assert!(
            r >= 0.9 - EPS && r <= 1.0 + EPS,
            "vertex outside the wall thickness: {p:?}"
        );
    }
    // End caps sit flush on both axes.
    assert!(has_vertex(&positions, [1.0, 0.0, 0.0]));
    assert!(has_vertex(&positions, [0.0, 1.0, 0.9]));
}

#[test]
fn l_shape_notch_is_empty() {
    let mesh = create_l_shape_mesh();
    let positions = positions_of(&mesh);
    assert_bounds("l_shape", &positions, [-0.5, -0.5, -0.15], [0.5, 0.5, 0.15]);
    // The notch (above and right of the 0.3-wide arms) holds no geometry;
    // the inner corner sits exactly at (-0.2, -0.2) after centering.
    for p in &positions {
        assert!(
            !(p[0] > -0.2 + EPS && p[1] > -0.2 + EPS),
            "vertex inside the notch: {p:?}"
        );
    }
    assert!(
        has_vertex(&positions, [-0.2, -0.2, 0.15]),
        "missing inner corner"
    );
}

#[test]
fn t_shape_regions_beside_stem_are_empty() {
    let mesh = create_t_shape_mesh();
    let positions = positions_of(&mesh);
    assert_bounds("t_shape", &positions, [-0.5, -0.5, -0.15], [0.5, 0.5, 0.15]);
    // Below the bar (Y < 0.2 centered) only the 0.3-wide stem remains.
    for p in &positions {
        if p[1] < 0.2 - EPS {
            assert!(p[0].abs() <= 0.15 + EPS, "vertex beside the stem: {p:?}");
        }
    }
}

#[test]
fn cross_shape_is_mirror_symmetric() {
    let mesh = create_cross_shape_mesh();
    let positions = positions_of(&mesh);
    assert_bounds(
        "cross_shape",
        &positions,
        [-0.5, -0.5, -0.15],
        [0.5, 0.5, 0.15],
    );
    for p in &positions {
        assert!(
            has_vertex(&positions, [-p[0], p[1], p[2]]),
            "missing X mirror of {p:?}"
        );
        assert!(
            has_vertex(&positions, [p[0], -p[1], p[2]]),
            "missing Y mirror of {p:?}"
        );
        // The four corner regions between the arms stay empty.
        assert!(
            !(p[0].abs() > 0.15 + EPS && p[1].abs() > 0.15 + EPS),
            "vertex in a corner region: {p:?}"
        );
    }
}
