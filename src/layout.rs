#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    One,
    /// Stacked dual view (2×1 — vertical split).
    Two,
    Grid2,
    Grid3,
    Grid4,
    Grid5,
    Grid6,
}

impl Layout {
    pub fn all() -> &'static [Self] {
        &[
            Self::One,
            Self::Two,
            Self::Grid2,
            Self::Grid3,
            Self::Grid4,
            Self::Grid5,
            Self::Grid6,
        ]
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "1" | "1x1" => Self::One,
            "2" | "2x1" | "1x2" => Self::Two,
            "2x2" => Self::Grid2,
            "4x4" => Self::Grid4,
            "5x5" => Self::Grid5,
            "6x6" => Self::Grid6,
            _ => Self::Grid3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::Two => "2",
            Self::Grid2 => "2x2",
            Self::Grid3 => "3x3",
            Self::Grid4 => "4x4",
            Self::Grid5 => "5x5",
            Self::Grid6 => "6x6",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::Two => "2",
            Self::Grid2 => "2×2",
            Self::Grid3 => "3×3",
            Self::Grid4 => "4×4",
            Self::Grid5 => "5×5",
            Self::Grid6 => "6×6",
        }
    }

    pub fn cols(self) -> usize {
        match self {
            Self::One | Self::Two => 1,
            Self::Grid2 => 2,
            Self::Grid3 => 3,
            Self::Grid4 => 4,
            Self::Grid5 => 5,
            Self::Grid6 => 6,
        }
    }

    pub fn rows(self) -> usize {
        match self {
            Self::One => 1,
            Self::Two | Self::Grid2 => 2,
            Self::Grid3 => 3,
            Self::Grid4 => 4,
            Self::Grid5 => 5,
            Self::Grid6 => 6,
        }
    }

    pub fn cells(self) -> usize {
        self.cols() * self.rows()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Contain,
    Cover,
    Fill,
}

impl FitMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "cover" => Self::Cover,
            "fill" | "stretch" => Self::Fill,
            _ => Self::Contain,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Fill => "fill",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Contain => "Contain",
            Self::Cover => "Cover",
            Self::Fill => "Fill",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Contain, Self::Cover, Self::Fill]
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Contain => Self::Cover,
            Self::Cover => Self::Fill,
            Self::Fill => Self::Contain,
        }
    }
}
