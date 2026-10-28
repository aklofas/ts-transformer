"""mypy --strict: the transport modules' exceptions ARE tstrans.exceptions'
classes (runtime re-exports them; the stubs must say so). Static-only."""
from typing import assert_type
import tstrans.exceptions as exc
import tstrans.hls as hls
import tstrans.tcp as tcp
import tstrans.udp as udp

def takes_udp(e: exc.UdpError) -> None: ...
def takes_base(e: exc.TstError) -> None: ...

u: udp.UdpError
takes_udp(u)
takes_base(u)
t: tcp.TcpError
takes_base(t)
h: hls.HlsError
takes_base(h)
def udp_kind(k: udp.UdpErrorKind) -> None:
    assert_type(k, exc.UdpErrorKind)
udp_cls: type[exc.UdpError] = udp.UdpError
tcp_cls: type[exc.TcpError] = tcp.TcpError
hls_cls: type[exc.HlsError] = hls.HlsError
# Wrong-domain kinds stay rejected (warn_unused_ignores under strict):
exc.UdpError(kind=exc.TcpErrorKind.IO, message="x")  # type: ignore[arg-type]
exc.TcpError(kind=exc.HlsErrorKind.IO, message="x")  # type: ignore[arg-type]
exc.HlsError(kind=exc.UdpErrorKind.IO, message="x")  # type: ignore[arg-type]
