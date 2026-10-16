@dataclass(frozen=True, slots=True)
class WavelengthRecord:
    """One record of Item 128, Wavelengths List (ST 0601.19 §8.128) — a sensor wavelength band definition. `min_nm`/`max_nm` are ST 1201.5 IMAPB(0,1e9,4)-encoded, giving ~½ nm precision across the full X-ray-to-VHF spectrum span the spec cites."""

    id: int
    min_nm: float
    max_nm: float
    name: str
