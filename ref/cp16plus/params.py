"""CP16+ 参数表与常量（协议 v1，冻结值）。

所有频率均为音频域标称值。参考实现统一在 8 kHz 采样率上工作：
最高音 1250 + 保护带 < 4000，满足奈奎斯特；实际产品在 48 kHz 上做同样的事。
"""

from __future__ import annotations

from dataclasses import dataclass

FS = 8000                 # 参考实现采样率
FS_PROD = 48000           # 产品采样率

# --- 频段规划（默认布局） ---
PILOT1 = 450.0            # 导频 P1
PILOT2 = 1250.0           # 导频 P2
PILOT_SPAN = PILOT2 - PILOT1          # 800 Hz = 32·Δf

CC_LO, CC_HI = 500.0, 740.0
CC_CENTER = 0.5 * (CC_LO + CC_HI)     # 620 Hz
CC_BAUD = 125.0
CC_RRC_ALPHA = 0.35
CC_SPS = FS / CC_BAUD                 # 64 samples/symbol @8k

VIS_LO_DEFAULT = 800.0

# --- 共存布局（全部压到 1000 Hz 以下） ---
COEXIST = {
    "pilot1": 350.0,
    "pilot2": 960.0,
    "cc_center": 500.0,
    "f_lo": 640.0,
    "df": 18.75,
}

# --- 时间结构（秒） ---
LEAD_PILOT = 1.5          # 导频领先
GAP_HDR_CELL = 0.10       # 兼容旧值；新实现用 SEGMENT_GAP_SYM（符号数）锚定
TAIL_PILOT = 0.5          # 结束后导频尾巴
CELL_MARGIN_ROWS = 1      # 每字符 1 行边距
ROW_RAMP = 0.03           # 每路包络升余弦过渡，占 T_row 的比例

# --- CC 锚定定时（协议常量，见规范 §6.3）---
CC_SYM = 1.0 / CC_BAUD                 # 8 ms
SEGMENT_GAP_SYM = 20                   # 突发结束到本段第一行的间隔（符号）；须 > 滤波器拖尾 16
HDR_SYMBOLS = 32 + 171                 # 前导 + 数据符号
BCON_SYMBOLS = 32 + 68
EOT_SYMBOLS = 32 + 22
CELLS_PER_SEGMENT = 8                  # 每段最多 8 个字符（产品值）

# --- 控制信道帧 ---
HDR_BITS = 165
BCON_BITS = 62
EOT_BITS = 16
FRAME_MAGIC = 0x4350      # "CP"
PROTO_VER = 1


@dataclass(frozen=True)
class Mode:
    """一个视觉层档位。"""

    name: str
    n: int                # 栅格边长（16 / 24）
    df: float             # 音间隔
    k: int                # Δf·T_row
    repeat: int = 1       # 行重复
    f_lo: float = VIS_LO_DEFAULT

    @property
    def t_row(self) -> float:
        return self.k / self.df

    @property
    def rows_per_char(self) -> int:
        return (CELL_MARGIN_ROWS + self.n) * self.repeat

    @property
    def row_symbols(self) -> int:
        """一行等于多少个 CC 符号——CC 锚定定时的核心整数关系（T_row = 5k·T_sym）。"""
        return 5 * self.k

    @property
    def gap_symbols(self) -> int:
        return SEGMENT_GAP_SYM

    @property
    def band(self) -> float:
        return self.n * self.df

    @property
    def char_time(self) -> float:
        return self.rows_per_char * self.t_row

    @property
    def f_hi(self) -> float:
        return self.f_lo + (self.n - 1) * self.df

    @property
    def f_margin(self) -> float:
        """边距点所在频率（视觉带下方 2 个音）。"""
        return self.f_lo - 2 * self.df

    def tones(self) -> list[float]:
        return [self.f_lo + i * self.df for i in range(self.n)]


M1 = Mode("M1", 16, 25.0, 1)
M2 = Mode("M2", 16, 25.0, 2)
M4 = Mode("M4", 16, 25.0, 4)
M8 = Mode("M8", 16, 25.0, 8)
M16N4 = Mode("16N-M4", 16, 18.75, 4, f_lo=640.0)
M24_4 = Mode("24-M4", 24, 25.0, 4)

MODES = {m.name: m for m in (M1, M2, M4, M8, M16N4, M24_4)}
DEFAULT_MODE = "M2"

#: 参考带宽：像素信噪比增益以它为基准
B_REF = 2400.0


def by_name(name: str) -> Mode:
    if name not in MODES:
        raise KeyError(f"unknown mode {name!r}; known: {', '.join(MODES)}")
    return MODES[name]


def plan_of(mode: Mode) -> int:
    """由档位反推频段布局：0=默认（800/25），1=共存（640/18.75）。

    发射端必须把这一位写进 HDR，否则接收端会拿默认布局去套共存波形的栅格。
    """
    if abs(mode.f_lo - 800.0) < 1e-9 and abs(mode.df - 25.0) < 1e-9:
        return 0
    return 1


def pixel_snr_gain(mode: Mode) -> float:
    """像素判决信噪比相对 B_REF 内信噪比的改善量（dB）。"""
    import math

    return 10 * math.log10(2 * mode.k * B_REF / mode.band)
