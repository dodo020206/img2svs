"""独立转换 worker 入口，由 GUI 进程启动。"""

from __future__ import annotations

from typing import Sequence

from img2svs.app import convert_to_svs
from img2svs.core.svs_common import run_conversion_jobs


def main(argv: Sequence[str] | None = None) -> int:
    options = convert_to_svs.parse_args(argv)
    run_conversion_jobs(convert_to_svs.build_jobs(options))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
