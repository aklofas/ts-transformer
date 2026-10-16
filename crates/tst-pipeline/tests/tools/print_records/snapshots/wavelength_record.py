@dataclass(frozen=True, slots=True)
class WavelengthRecord:
    """One record of Item 128, Wavelengths List (ST 0601.19 §8.128) — a"""

    id: int
    min_nm: float
    max_nm: float
    name: str
