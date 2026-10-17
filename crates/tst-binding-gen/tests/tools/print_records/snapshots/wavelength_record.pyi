@dataclass(frozen=True, slots=True)
class WavelengthRecord:
    id: int
    min_nm: float
    max_nm: float
    name: str
