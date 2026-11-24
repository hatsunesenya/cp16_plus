"""CP16+ 参考实现（协议 v1）。

模块划分与架构文档 §4.2 对应：
    params    模式表与常量
    font      字模（HZK16 / CPF / 系统字体）
    vision    视觉层调制与解调
    pilot     双导频仿射标定
    modem     CC 物理层（DQPSK + RRC）
    fec       卷积码、交织、CRC
    frame     HDR / BCON / EOT
    tx / rx   收发流水线
    metrics   可读性代理指标 J
    display   瀑布渲染
    channel   信道模拟（时钟偏差 / 频偏 / 衰落 / 噪声）
"""

__version__ = "1.0.0"
__proto__ = 1
