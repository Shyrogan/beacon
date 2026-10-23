//! Per-state light properties, extracted from vanilla by `extractor/`.

use chunkedge::BlockState;

/// How a block state interacts with light passing through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightProperties {
    state: &'static StateLight,
}

impl LightProperties {
    pub fn of(state: BlockState) -> Self {
        Self {
            state: &STATES[state.to_raw() as usize],
        }
    }

    /// The light this block gives off, `0..=15`.
    pub fn emission(self) -> u8 {
        self.state.emission
    }

    /// Light lost when passing through this block, `0..=15`. Light always
    /// loses at least one level per block on top of this.
    pub fn dampening(self) -> u8 {
        self.state.dampening
    }

    /// Whether sky light passes straight down through this block without
    /// losing a level.
    pub fn propagates_skylight_down(self) -> bool {
        self.state.flags & PROPAGATES_SKYLIGHT_DOWN != 0
    }

    /// Whether light crossing this block depends on its face shapes, as for
    /// slabs and stairs (vanilla's `!isEmptyShape`).
    pub fn is_conditionally_opaque(self) -> bool {
        self.state.flags & USES_SHAPE_FOR_OCCLUSION != 0
    }

    /// The shape this state occludes light with on `face`, or `None` when the
    /// state isn't conditionally opaque.
    pub fn occlusion(self, face: Face) -> Option<&'static FaceMask> {
        self.is_conditionally_opaque()
            .then(|| &FACES[self.state.faces[face as usize] as usize])
    }

    /// Whether light crossing from `self` to `to` through `face` (a face of
    /// `self`) is blocked by the two touching face shapes. Vanilla's
    /// `LightEngine.shapeOccludes`.
    pub fn occludes(self, to: Self, face: Face) -> bool {
        faces_occlude(self.occlusion(face), to.occlusion(face.opposite()))
    }
}

/// Whether two touching faces together cover the whole square between two
/// blocks, `None` being an empty face. Vanilla's `Shapes.faceShapeOccludes`.
#[inline]
pub fn faces_occlude(a: Option<&FaceMask>, b: Option<&FaceMask>) -> bool {
    match (a, b) {
        (None, None) => false,
        (Some(face), None) | (None, Some(face)) => face.is_full(),
        (Some(a), Some(b)) => a.union_is_full(b),
    }
}

/// One side of a block, in vanilla's `Direction` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Face {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

impl Face {
    pub const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];

    pub fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }

    /// Unit offset `(x, y, z)` towards the neighbour across this face.
    pub fn offset(self) -> (i32, i32, i32) {
        match self {
            Self::Down => (0, -1, 0),
            Self::Up => (0, 1, 0),
            Self::North => (0, 0, -1),
            Self::South => (0, 0, 1),
            Self::West => (-1, 0, 0),
            Self::East => (1, 0, 0),
        }
    }
}

/// A face shape sampled on the 16x16 pixel grid of the face plane.
#[derive(Debug, PartialEq, Eq)]
pub struct FaceMask([u64; 4]);

impl FaceMask {
    fn is_full(&self) -> bool {
        self.0 == [u64::MAX; 4]
    }

    fn union_is_full(&self, other: &Self) -> bool {
        (0..4).all(|i| self.0[i] | other.0[i] == u64::MAX)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StateLight {
    dampening: u8,
    flags: u8,
    emission: u8,
    faces: [u8; 6],
}

const PROPAGATES_SKYLIGHT_DOWN: u8 = 1;
const USES_SHAPE_FOR_OCCLUSION: u8 = 2;

include!(concat!(env!("OUT_DIR"), "/light_tables.rs"));
