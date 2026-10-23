//! Light values of lit chunks, one nibble per block.

/// The sky and block light of one chunk column, for every light section: the
/// world's sections plus one below and one above. Remembers which sections
/// changed since clients were last updated.
#[derive(Clone, Debug)]
pub struct ChunkLight {
    /// Empty in dimensions without sky light.
    sky: Box<[Nibbles]>,
    block: Box<[Nibbles]>,
    dirty: [SectionSet; 2],
}

impl ChunkLight {
    /// A dark chunk with every section marked dirty, so its first send is
    /// complete.
    pub fn new(light_sections: usize, has_sky: bool) -> Self {
        let sky_sections = if has_sky { light_sections } else { 0 };
        Self {
            sky: vec![Nibbles::uniform(0); sky_sections].into(),
            block: vec![Nibbles::uniform(0); light_sections].into(),
            dirty: [
                SectionSet::full(sky_sections),
                SectionSet::full(light_sections),
            ],
        }
    }

    pub fn sections(&self, kind: LightKind) -> &[Nibbles] {
        match kind {
            LightKind::Sky => &self.sky,
            LightKind::Block => &self.block,
        }
    }

    #[inline]
    pub fn get(&self, kind: LightKind, section: usize, index: usize) -> u8 {
        self.sections(kind)[section].get(index)
    }

    pub fn mark_dirty(&mut self, kind: LightKind, section: usize) {
        self.dirty[kind as usize].insert(section);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.iter().any(|set| !set.is_empty())
    }

    pub fn dirty(&self, kind: LightKind) -> &SectionSet {
        &self.dirty[kind as usize]
    }

    /// Forgets which sections changed, and shrinks the ones that became
    /// uniform.
    pub fn clear_dirty(&mut self) {
        for kind in [LightKind::Sky, LightKind::Block] {
            let dirty = std::mem::take(&mut self.dirty[kind as usize]);
            for section in dirty.iter() {
                self.sections_mut(kind)[section].compact();
            }
        }
    }

    pub fn sections_mut(&mut self, kind: LightKind) -> &mut [Nibbles] {
        match kind {
            LightKind::Sky => &mut self.sky,
            LightKind::Block => &mut self.block,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightKind {
    Sky = 0,
    Block = 1,
}

/// The light of one 16x16x16 section, in vanilla's `DataLayer` layout:
/// index `x | z << 4 | y << 8`, even indices in the low nibble.
///
/// Both variants hold a pointer to the bytes, so reading doesn't depend on
/// which one it is.
#[derive(Clone, Debug)]
pub enum Nibbles {
    /// Every block has the same level, one of the shared [`UNIFORM`] arrays.
    /// Most sections are all 0 or all 15.
    Uniform(&'static [u8; 2048]),
    Data(Box<[u8; 2048]>),
}

/// The bytes of a section where every block has level `i`.
static UNIFORM: [[u8; 2048]; 16] = {
    let mut uniform = [[0; 2048]; 16];
    let mut level = 0;
    while level < 16 {
        uniform[level] = [level as u8 * 0x11; 2048];
        level += 1;
    }
    uniform
};

impl Nibbles {
    pub fn uniform(level: u8) -> Self {
        Self::Uniform(&UNIFORM[level as usize])
    }

    /// The level of every block, if they all have the same.
    pub fn uniform_level(&self) -> Option<u8> {
        match self {
            Self::Uniform(bytes) => Some(bytes[0] & 0xF),
            Self::Data(_) => None,
        }
    }

    /// The 2048 bytes sent to clients.
    #[inline]
    pub fn bytes(&self) -> &[u8; 2048] {
        match self {
            Self::Uniform(bytes) => bytes,
            Self::Data(bytes) => bytes,
        }
    }

    #[inline]
    pub fn get(&self, index: usize) -> u8 {
        (self.bytes()[(index >> 1) & 0x7FF] >> ((index & 1) << 2)) & 0xF
    }

    /// Returns whether the level changed.
    #[inline]
    pub fn set(&mut self, index: usize, level: u8) -> bool {
        let bytes = match self {
            Self::Uniform(bytes) if bytes[0] & 0xF == level => return false,
            Self::Uniform(bytes) => {
                *self = Self::Data(Box::new(**bytes));
                let Self::Data(bytes) = self else {
                    unreachable!()
                };
                bytes
            }
            Self::Data(bytes) => bytes,
        };
        let shift = (index & 1) << 2;
        let byte = &mut bytes[(index >> 1) & 0x7FF];
        let old = *byte;
        *byte = (old & (0xF0 >> shift)) | (level << shift);
        *byte != old
    }

    fn compact(&mut self) {
        if let Self::Data(bytes) = self {
            let first = bytes[0];
            if first >> 4 == first & 0xF && bytes.iter().all(|&b| b == first) {
                *self = Self::uniform(first & 0xF);
            }
        }
    }
}

/// A set of section indices.
#[derive(Clone, Debug, Default)]
pub struct SectionSet(Vec<u64>);

impl SectionSet {
    fn full(len: usize) -> Self {
        let mut set = Self::default();
        (0..len).for_each(|section| set.insert(section));
        set
    }

    pub fn insert(&mut self, section: usize) {
        let word = section / 64;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (section % 64);
    }

    pub fn contains(&self, section: usize) -> bool {
        self.0
            .get(section / 64)
            .is_some_and(|word| word & (1 << (section % 64)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &word)| {
            (0..64)
                .filter(move |bit| word & (1 << bit) != 0)
                .map(move |bit| i * 64 + bit)
        })
    }
}
