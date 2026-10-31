# BLF 文件 Rust 解析核心与 Python 接口实现方案

## 1. 目标

提供 Python 可用的 BLF 文件解析能力，用于 ECU 自动化测试脚本。热路径放在 Rust，Python 只暴露接口。

本次方案以 `Technica-Engineering/vector_blf` 为格式与对象模型基准，目标是：

1. 类名、字段名、对象类型枚举与 `vector_blf` 尽量一致。
2. 文件头、对象头、日志容器、restore point 的解析边界与 `vector_blf` 一致。
3. Rust 提供核心解析，PyO3 提供 Python 接口。
4. 保留 `cantools` 的 DBC 解码协作方式，但不把 DBC 解析放进 Rust 核心。

## 2. 对照结论

### 2.1 vector_blf

仓库：[Technica-Engineering/vector_blf](https://github.com/Technica-Engineering/vector_blf)

对照源码后确认：

- 项目实现语言是 C++，不是 Rust。
- 许可证是 `GPL-3.0-or-later`，不能把源码直接翻译或复制进 MIT/Apache 项目。
- README 声明兼容 `binlog API version 7.1.0`。
- 当前实现主要支持 little-endian。
- 顶层入口是 `Vector::BLF::File`，公开属性包括 `fileStatistics`、`currentUncompressedFileSize`、`currentObjectCount`、`compressionLevel`、`writeRestorePoints`。
- 核心读取流程是 `CompressedFile -> LogContainer -> UncompressedFile -> ObjectHeaderBase::read -> createObject -> read`。
- 对象头扫描会查找 `LOBJ` 签名，并处理对象之间可能出现的错位或 padding。
- 未知对象在 `createObject` 返回空指针时按 `objectSize` 跳过。

### 2.2 cantools

仓库：[cantools/cantools](https://github.com/cantools/cantools)

- 使用 Python，许可证 MIT。
- 强项是 DBC、KCD、ARXML 解析和信号 decode/encode。
- `cantools.logreader` 解析文本日志，不解析 BLF。
- 本项目参考它的 Python 脚本使用方式，不复制其日志解析模型。

### 2.3 现有 Rust 候选

| crate | 许可证 | 说明 |
| --- | --- | --- |
| `ablf` | MIT OR Apache-2.0 | clean-room 实现，但对象覆盖少，适合原型验证 |
| `autors-blf` | Apache-2.0 | 描述为 BLF 读写，生态较新 |
| `blf_asc` | Apache-2.0 | 覆盖有限，不建议作为长期基础 |

建议以 `vector_blf` 为格式基准自研 Rust `blf-core`。`ablf` 可在技术验证阶段作为 clean-room 交叉参考，不作为最终模型来源。

## 3. 对齐原则

为了让 Python 用户和已有 C++ 调试经验一致，采用以下规则：

1. Rust 内部结构体名称尽量与 C++ 类名一致，例如 `CanMessage`、`CanMessage2`、`CanFdMessage64`。
2. Python 对象暴露与 C++ 相同的字段名，例如 `channel`、`flags`、`dlc`、`id`、`data`、`objectTimeStamp`。
3. Python 对象暴露与 C++ 相同的顶层类型名，例如 `File`、`FileStatistics`、`CanMessage`。
4. Rust 内部字段名可使用 `snake_case`，但 PyO3 getter 必须映射为 C++ 原名。
5. 原始时间戳字段保留 C++ 语义，不擅自归一化。可另外提供只读的 `timestamp_ns` 便捷属性。
6. 未知对象默认按 `objectSize` 跳过，与 C++ `createObject` 返回空指针后的行为一致；严格模式可改为抛出异常。
7. 文件读取支持只读方向，优先实现解析。写 BLF 暂不纳入第一版。

## 4. 文件格式结构

### 4.1 文件统计头

签名是 `LOGG`，即 `0x47474F4C`。

`FileStatistics` 字段顺序与 `vector_blf` 保持一致：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `signature` | `u32` | `LOGG` |
| `statisticsSize` | `u32` | 当前版本固定读取 144 字节布局 |
| `apiNumber` | `u32` | `major*1_000_000 + minor*1_000 + build*100 + patch` |
| `applicationId` | `u8` | CANalyzer、CANoe 等 |
| `compressionLevel` | `u8` | 0 到 10，10 实际映射到 zlib 6 |
| `applicationMajor` | `u8` | 应用主版本 |
| `applicationMinor` | `u8` | 应用次版本 |
| `fileSize` | `u64` | 压缩后文件大小 |
| `uncompressedFileSize` | `u64` | 解压后总大小 |
| `objectCount` | `u32` | 对象数量 |
| `applicationBuild` | `u32` | 应用 build |
| `measurementStartTime` | `SYSTEMTIME` | 8 个 `u16` |
| `lastObjectTime` | `SYSTEMTIME` | 8 个 `u16` |
| `restorePointsOffset` | `u64` | restore point 容器所在位置 |
| `reservedFileStatistics` | `[u32; 16]` | 64 字节 |

`SYSTEMTIME` 字段顺序是：

```text
year, month, dayOfWeek, day, hour, minute, second, milliseconds
```

`vector_blf` 的 `FileStatistics::read()` 不会根据 `statisticsSize` 动态缩短读取，而是直接读取完整 144 字节布局，也不校验该字段。第一版应保持同样行为：读取并记录 `statisticsSize`，但不把它作为硬错误条件，只用于文件统计和兼容性告警。

源码中的默认 `apiNumber` 是 `4080200`，表示 `4.8.2.0`。README 中提到的 `binlog API version 7.1.0` 是兼容级别声明，两者含义不同，文档和测试不要混用。

### 4.2 对象头

`ObjectSignature` 是 `LOBJ`，即 `0x4A424F4C`。

`ObjectHeaderBase` 公共字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `signature` | `u32` | `LOBJ` |
| `headerSize` | `u16` | 对象头字节数 |
| `headerVersion` | `u16` | 1 或 2 |
| `objectSize` | `u32` | 完整对象字节数 |
| `objectType` | `u32` | 对应 `ObjectType` |

`ObjectHeader` 对应 version 1，追加：

| 字段 | 类型 |
| --- | --- |
| `objectFlags` | `u32` |
| `clientIndex` | `u16` |
| `objectVersion` | `u16` |
| `objectTimeStamp` | `u64` |

`ObjectHeader2` 对应 version 2，追加：

| 字段 | 类型 |
| --- | --- |
| `objectFlags` | `u32` |
| `timeStampStatus` | `u8` |
| `reservedObjectHeader` | `u8` |
| `objectVersion` | `u16` |
| `objectTimeStamp` | `u64` |
| `originalTimeStamp` | `u64` |

`VarObjectHeader` 也使用 version 1，但第二个 `u16` 是 `objectStaticSize`，不是 `clientIndex`。

当前 `vector_blf` 中没有具体对象继承 `VarObjectHeader`，该类型保留给未来动态扩展对象使用。Rust 可以先实现结构，但不为具体 `objectType` 分派它。

时间单位由 `objectFlags` 决定：

- `TimeTenMics = 0x00000001`
- `TimeOneNans = 0x00000002`

### 4.3 LogContainer

`LogContainer` 继承 `ObjectHeaderBase`，`objectType` 为 `LOG_CONTAINER`，没有 `ObjectHeader` 的 timestamp 扩展字段。

字段顺序：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `compressionMethod` | `u16` | 0 不压缩，2 zlib/deflate |
| `reservedLogContainer1` | `u16` | 保留 |
| `reservedLogContainer2` | `u32` | 保留 |
| `uncompressedFileSize` | `u32` | 解压后大小 |
| `reservedLogContainer3` | `u32` | 保留 |
| `compressedFile` | 字节流 | 压缩数据 |

容器头与数据关系：

```text
internalHeaderSize = ObjectHeaderBase 16 + 2 + 2 + 4 + 4 + 4
compressedFileSize = objectSize - internalHeaderSize
```

读取完 `compressedFile` 后，`vector_blf` 还会跳过 `objectSize % 4` 字节 padding。Rust 实现必须保留这个边界。

### 4.4 RestorePoint

`RestorePoint` 每个对象 24 字节：

| 字段 | 类型 |
| --- | --- |
| `timeStamp` | `u64` |
| `compressedFilePosition` | `u64` |
| `uncompressedFileOffset` | `u32` |
| `unknownRestorePoint` | `u32` |

`RestorePointContainer` 的 `objectType` 为 `Unknown115`，继承 `ObjectHeader`，字段包括：

```text
reservedRestorePointContainer: [u8; 14]
dataLength: u16
data: Vec<u8>
```

`RestorePointContainer.data` 内部实际是 `RestorePoints` 的序列化内容：

```text
objectSize: u32
objectInterval: u32
restorePoints: Vec<RestorePoint>
```

`RestorePoints` 元素数量按 `(objectSize - 8) / 24` 计算，其中 8 是前面两个 `u32` 字段大小，24 是单个 `RestorePoint` 大小。

### 4.5 关键实现细节

以下行为来自 `vector_blf` 的实际读取代码，Rust 实现需要保持一致：

1. `ObjectHeaderBase::read()` 不是简单顺序读取，而是从当前位置扫描 `LOBJ` 签名。遇到部分签名时会回退 1、2 或 3 字节继续扫描。Rust 应实现等价的稳健扫描，但必须限制最大扫描范围，避免损坏文件导致无限扫描。限制扫描范围是安全增强，与 C++ 扫到 EOF 后抛错的行为不同，应作为有意差异记录。
2. `File::read()` 返回的是解压后逻辑流中的对象，不返回 `LogContainer`。`LogContainer` 只是文件外层载体。
3. `File::read()` 可能返回 `Unknown115` 对应的 `RestorePointContainer`。C++ 不会把它自动隐藏，只是不计入 `currentObjectCount`。
4. 未知 `objectType` 在 `createObject()` 返回空指针时，按 `objectSize` 直接跳过。
5. 可变长对象的长度规则不是统一的。`CanMessage2::data` 和 `CanErrorFrameExt::data` 使用 `objectSize - calculateObjectSize()`；`CanFdMessage64::data` 和 `CanFdErrorFrame64::data` 使用 `validDataBytes`；它们的尾部保留字节 `reservedCanFdExtFrameData` 才使用 `objectSize - calculateObjectSize()`。Rust 不能把这些对象简单定义为完全固定大小。
6. `CanErrorFrame` 的 `reservedCanErrorFrame` 只在 `length > 0` 时存在。
7. 对象头类型必须按 `objectType` 分派，不能只按 `headerVersion` 判断。`LogContainer` 虽然 `headerVersion == 1`，但只有 16 字节 `ObjectHeaderBase` 头。MOST 对象中，`MostPkt` 仍使用 `ObjectHeader`，从 `MostPkt2` 开始才切换为 `ObjectHeader2`。
8. `CanFdMessage64` 和 `CanFdErrorFrame64` 使用 `extDataOffset` 和 `objectSize` 判断是否读取 `CanFdExtFrameData`。
9. `CanFdErrorFrame64` 声明的 `reservedCanFdErrorFrame64` 在 C++ `read()`/`write()` 中没有实际读写，是死字段。真实尾部保留字节是 `CanFdExtFrameData::reservedCanFdExtFrameData`。Rust 类型不要把这个死字段映射到二进制流。
10. 已知但尚未实现的对象类型，在非严格模式下也要按 `objectSize` 跳过，以保持 `File::read()` 对包含 P1/P2 对象的文件兼容；不能把“未实现”误当格式错误。

## 5. 类与属性映射

### 5.1 顶层文件类

Python 侧提供与 C++ `File` 对应的只读接口：

| Python 属性或方法 | 对应 C++ |
| --- | --- |
| `File.fileStatistics` | `fileStatistics` |
| `File.currentUncompressedFileSize` | `currentUncompressedFileSize` |
| `File.currentObjectCount` | `currentObjectCount` |
| `File.compressionLevel` | `compressionLevel` |
| `File.writeRestorePoints` | `writeRestorePoints` |
| `File.open(path, mode)` | `open(path, mode)` |
| `File.is_open()` | `is_open()` |
| `File.good()` | `good()` |
| `File.eof()` | `eof()` |
| `File.read()` | `read()` |
| `File.close()` | `close()` |

第一版只支持读模式，`mode` 默认 `"r"`。

`File.read()` 的返回语义：

- 返回解压后逻辑流中的下一个对象。
- 返回 `None` 表示 EOF。
- `LogContainer` 不会作为 `read()` 结果返回。
- `Unknown115` 对应的 `RestorePointContainer` 会作为 `read()` 结果返回。
- 未支持对象默认按 `objectSize` 跳过。
- `currentObjectCount` 不统计 `Unknown115`。

错误行为需要和 C++ 分开建模：

- 未知对象：默认跳过；严格模式抛 `BlfUnsupportedObjectError`。
- 截断对象或读到逻辑流末尾之后：C++ 抛异常；第一版默认同样抛 `BlfTruncatedError`，非严格模式再允许安全停止。

`good()` 和 `eof()` 在 C++ 中由 `ObjectQueue` 驱动，不直接等同于底层流状态。Python 的可靠 EOF 判断应以 `read() is None` 为准，`good()`/`eof()` 只作为辅助状态。

### 5.2 对象类型到类的映射

下表按 `ObjectType` 数值列出对应的 `vector_blf` 类：

| ID | ObjectType | C++ 类 |
| --- | --- | --- |
| 1 | `CAN_MESSAGE` | `CanMessage` |
| 2 | `CAN_ERROR` | `CanErrorFrame` |
| 3 | `CAN_OVERLOAD` | `CanOverloadFrame` |
| 4 | `CAN_STATISTIC` | `CanDriverStatistic` |
| 5 | `APP_TRIGGER` | `AppTrigger` |
| 6 | `ENV_INTEGER` | `EnvironmentVariable` |
| 7 | `ENV_DOUBLE` | `EnvironmentVariable` |
| 8 | `ENV_STRING` | `EnvironmentVariable` |
| 9 | `ENV_DATA` | `EnvironmentVariable` |
| 10 | `LOG_CONTAINER` | `LogContainer` |
| 11 | `LIN_MESSAGE` | `LinMessage` |
| 12 | `LIN_CRC_ERROR` | `LinCrcError` |
| 13 | `LIN_DLC_INFO` | `LinDlcInfo` |
| 14 | `LIN_RCV_ERROR` | `LinReceiveError` |
| 15 | `LIN_SND_ERROR` | `LinSendError` |
| 16 | `LIN_SLV_TIMEOUT` | `LinSlaveTimeout` |
| 17 | `LIN_SCHED_MODCH` | `LinSchedulerModeChange` |
| 18 | `LIN_SYN_ERROR` | `LinSyncError` |
| 19 | `LIN_BAUDRATE` | `LinBaudrateEvent` |
| 20 | `LIN_SLEEP` | `LinSleepModeEvent` |
| 21 | `LIN_WAKEUP` | `LinWakeupEvent` |
| 22 | `MOST_SPY` | `MostSpy` |
| 23 | `MOST_CTRL` | `MostCtrl` |
| 24 | `MOST_LIGHTLOCK` | `MostLightLock` |
| 25 | `MOST_STATISTIC` | `MostStatistic` |
| 29 | `FLEXRAY_DATA` | `FlexRayData` |
| 30 | `FLEXRAY_SYNC` | `FlexRaySync` |
| 31 | `CAN_DRIVER_ERROR` | `CanDriverError` |
| 32 | `MOST_PKT` | `MostPkt` |
| 33 | `MOST_PKT2` | `MostPkt2` |
| 34 | `MOST_HWMODE` | `MostHwMode` |
| 35 | `MOST_REG` | `MostReg` |
| 36 | `MOST_GENREG` | `MostGenReg` |
| 37 | `MOST_NETSTATE` | `MostNetState` |
| 38 | `MOST_DATALOST` | `MostDataLost` |
| 39 | `MOST_TRIGGER` | `MostTrigger` |
| 40 | `FLEXRAY_CYCLE` | `FlexRayV6StartCycleEvent` |
| 41 | `FLEXRAY_MESSAGE` | `FlexRayV6Message` |
| 42 | `LIN_CHECKSUM_INFO` | `LinChecksumInfo` |
| 43 | `LIN_SPIKE_EVENT` | `LinSpikeEvent` |
| 44 | `CAN_DRIVER_SYNC` | `CanDriverHwSync` |
| 45 | `FLEXRAY_STATUS` | `FlexRayStatusEvent` |
| 46 | `GPS_EVENT` | `GpsEvent` |
| 47 | `FR_ERROR` | `FlexRayVFrError` |
| 48 | `FR_STATUS` | `FlexRayVFrStatus` |
| 49 | `FR_STARTCYCLE` | `FlexRayVFrStartCycle` |
| 50 | `FR_RCVMESSAGE` | `FlexRayVFrReceiveMsg` |
| 51 | `REALTIMECLOCK` | `RealtimeClock` |
| 54 | `LIN_STATISTIC` | `LinStatisticEvent` |
| 55 | `J1708_MESSAGE` | `J1708Message` |
| 56 | `J1708_VIRTUAL_MSG` | `J1708Message` |
| 57 | `LIN_MESSAGE2` | `LinMessage2` |
| 58 | `LIN_SND_ERROR2` | `LinSendError2` |
| 59 | `LIN_SYN_ERROR2` | `LinSyncError2` |
| 60 | `LIN_CRC_ERROR2` | `LinCrcError2` |
| 61 | `LIN_RCV_ERROR2` | `LinReceiveError2` |
| 62 | `LIN_WAKEUP2` | `LinWakeupEvent2` |
| 63 | `LIN_SPIKE_EVENT2` | `LinSpikeEvent2` |
| 64 | `LIN_LONG_DOM_SIG` | `LinLongDomSignalEvent` |
| 65 | `APP_TEXT` | `AppText` |
| 66 | `FR_RCVMESSAGE_EX` | `FlexRayVFrReceiveMsgEx` |
| 67 | `MOST_STATISTICEX` | `MostStatisticEx` |
| 68 | `MOST_TXLIGHT` | `MostTxLight` |
| 69 | `MOST_ALLOCTAB` | `MostAllocTab` |
| 70 | `MOST_STRESS` | `MostStress` |
| 71 | `ETHERNET_FRAME` | `EthernetFrame` |
| 72 | `SYS_VARIABLE` | `SystemVariable` |
| 73 | `CAN_ERROR_EXT` | `CanErrorFrameExt` |
| 74 | `CAN_DRIVER_ERROR_EXT` | `CanDriverErrorExt` |
| 75 | `LIN_LONG_DOM_SIG2` | `LinLongDomSignalEvent2` |
| 76 | `MOST_150_MESSAGE` | `Most150Message` |
| 77 | `MOST_150_PKT` | `Most150Pkt` |
| 78 | `MOST_ETHERNET_PKT` | `MostEthernetPkt` |
| 79 | `MOST_150_MESSAGE_FRAGMENT` | `Most150MessageFragment` |
| 80 | `MOST_150_PKT_FRAGMENT` | `Most150PktFragment` |
| 81 | `MOST_ETHERNET_PKT_FRAGMENT` | `MostEthernetPktFragment` |
| 82 | `MOST_SYSTEM_EVENT` | `MostSystemEvent` |
| 83 | `MOST_150_ALLOCTAB` | `Most150AllocTab` |
| 84 | `MOST_50_MESSAGE` | `Most50Message` |
| 85 | `MOST_50_PKT` | `Most50Pkt` |
| 86 | `CAN_MESSAGE2` | `CanMessage2` |
| 87 | `LIN_UNEXPECTED_WAKEUP` | `LinUnexpectedWakeup` |
| 88 | `LIN_SHORT_OR_SLOW_RESPONSE` | `LinShortOrSlowResponse` |
| 89 | `LIN_DISTURBANCE_EVENT` | `LinDisturbanceEvent` |
| 90 | `SERIAL_EVENT` | `SerialEvent` |
| 91 | `OVERRUN_ERROR` | `DriverOverrun` |
| 92 | `EVENT_COMMENT` | `EventComment` |
| 93 | `WLAN_FRAME` | `WlanFrame` |
| 94 | `WLAN_STATISTIC` | `WlanStatistic` |
| 95 | `MOST_ECL` | `MostEcl` |
| 96 | `GLOBAL_MARKER` | `GlobalMarker` |
| 97 | `AFDX_FRAME` | `AfdxFrame` |
| 98 | `AFDX_STATISTIC` | `AfdxStatistic` |
| 99 | `KLINE_STATUSEVENT` | `KLineStatusEvent` |
| 100 | `CAN_FD_MESSAGE` | `CanFdMessage` |
| 101 | `CAN_FD_MESSAGE_64` | `CanFdMessage64` |
| 102 | `ETHERNET_RX_ERROR` | `EthernetRxError` |
| 103 | `ETHERNET_STATUS` | `EthernetStatus` |
| 104 | `CAN_FD_ERROR_64` | `CanFdErrorFrame64` |
| 105 | `LIN_SHORT_OR_SLOW_RESPONSE2` | `LinShortOrSlowResponse2` |
| 106 | `AFDX_STATUS` | `AfdxStatus` |
| 107 | `AFDX_BUS_STATISTIC` | `AfdxBusStatistic` |
| 109 | `AFDX_ERROR_EVENT` | `AfdxErrorEvent` |
| 110 | `A429_ERROR` | `A429Error` |
| 111 | `A429_STATUS` | `A429Status` |
| 112 | `A429_BUS_STATISTIC` | `A429BusStatistic` |
| 113 | `A429_MESSAGE` | `A429Message` |
| 114 | `ETHERNET_STATISTIC` | `EthernetStatistic` |
| 115 | `Unknown115` | `RestorePointContainer` |
| 118 | `TEST_STRUCTURE` | `TestStructure` |
| 119 | `DIAG_REQUEST_INTERPRETATION` | `DiagRequestInterpretation` |
| 120 | `ETHERNET_FRAME_EX` | `EthernetFrameEx` |
| 121 | `ETHERNET_FRAME_FORWARDED` | `EthernetFrameForwarded` |
| 122 | `ETHERNET_ERROR_EX` | `EthernetErrorEx` |
| 123 | `ETHERNET_ERROR_FORWARDED` | `EthernetErrorForwarded` |
| 124 | `FUNCTION_BUS` | `FunctionBus` |
| 125 | `DATA_LOST_BEGIN` | `DataLostBegin` |
| 126 | `DATA_LOST_END` | `DataLostEnd` |
| 127 | `WATER_MARK_EVENT` | `WaterMarkEvent` |
| 128 | `TRIGGER_CONDITION` | `TriggerCondition` |
| 129 | `CAN_SETTING_CHANGED` | `CanSettingChanged` |
| 130 | `DISTRIBUTED_OBJECT_MEMBER` | `DistributedObjectMember` |
| 131 | `ATTRIBUTE_EVENT` | `AttributeEvent` |

保留的 26、27、28、52、53、108、116、117 在 C++ 中不创建对象，解析时跳过。

### 5.3 头类型与继承关系

Rust 没有 C++ 多重继承，使用组合或 trait 来表达以下关系，但 Python 类名和字段名仍需保持一致。

使用 `ObjectHeader` 的对象是多数，包括：

```text
CanMessage, CanMessage2, CanFdMessage, CanFdMessage64,
CanErrorFrame, CanErrorFrameExt, CanFdErrorFrame64,
AppText, EnvironmentVariable, EthernetFrame, EthernetFrameEx,
LinMessage, LinMessage2, GpsEvent, SystemVariable
```

使用 `ObjectHeader2` 的主要是 MOST 后续对象。注意 `MostPkt` 仍使用 `ObjectHeader`，`MostPkt2` 才是 MOST 系列切到 `ObjectHeader2` 的边界：

```text
Most50Message, Most50Pkt, MostPkt2, MostHwMode, MostReg,
MostGenReg, MostNetState, MostDataLost, MostTrigger,
MostStatisticEx, MostTxLight, MostAllocTab, MostStress,
Most150Message, Most150Pkt, MostEthernetPkt,
Most150MessageFragment, Most150PktFragment,
MostEthernetPktFragment, MostSystemEvent, Most150AllocTab, MostEcl
```

其他混合基类：

```text
CanFdExtFrameData: CanFdMessage64, CanFdErrorFrame64
LinBusEvent: LinUnexpectedWakeup, LinSpikeEvent2, LinLongDomSignalEvent, LinLongDomSignalEvent2, LinWakeupEvent2
LinMessageDescriptor: LinSendError2
LinDatabyteTimestampEvent: LinMessage2, LinCrcError2, LinReceiveError2, LinShortOrSlowResponse, LinShortOrSlowResponse2
LinSynchFieldEvent: LinSyncError2
```

该混合基类清单只覆盖当前文档范围，P1 LIN 实现时必须以 C++ 头文件逐类核对，不作为完整清单。

`LogContainer` 只继承 `ObjectHeaderBase`。

### 5.4 关键 CAN 类字段

`CanMessage`：

```text
channel: u16
flags: u8
dlc: u8
id: u32
data: [u8; 8]
```

`CanMessage2`：

```text
channel: u16
flags: u8
dlc: u8
id: u32
data: Vec<u8>
frameLength: u32
bitCount: u8
reservedCanMessage1: u8
reservedCanMessage2: u16
```

`CanMessage2.data` 的长度不是固定值，应按 `objectSize - 固定字段大小` 计算。

`CanFdMessage`：

```text
channel: u16
flags: u8
dlc: u8
id: u32
frameLength: u32
arbBitCount: u8
canFdFlags: u8
validDataBytes: u8
reservedCanFdMessage1: u8
reservedCanFdMessage2: u32
data: [u8; 64]
reservedCanFdMessage3: u32
```

`CanFdMessage.data` 固定为 64 字节。

`CanFdMessage64`：

```text
channel: u8
dlc: u8
validDataBytes: u8
txCount: u8
id: u32
frameLength: u32
flags: u32
btrCfgArb: u32
btrCfgData: u32
timeOffsetBrsNs: u32
timeOffsetCrcDelNs: u32
bitCount: u16
dir: u8
extDataOffset: u8
crc: u32
data: Vec<u8>
```

`CanFdMessage64.data` 的长度由 `validDataBytes` 决定。当 `extDataOffset != 0` 且 `objectSize` 足够时，还需读取 `CanFdExtFrameData`：

```text
btrExtArb: u32
btrExtData: u32
reservedCanFdExtFrameData: Vec<u8>
```

`reservedCanFdExtFrameData` 的长度按 `objectSize - calculateObjectSize()` 计算。

`CanFdErrorFrame64` 的 `data` 同样由 `validDataBytes` 决定，扩展数据机制与 `CanFdMessage64` 相同。其 C++ 结构体中声明的 `reservedCanFdErrorFrame64` 在实际 `read()`/`write()` 中没有使用，真实尾部字节是 `reservedCanFdExtFrameData`，Rust 映射时不要引入 `reservedCanFdErrorFrame64` 作为二进制字段。

`CanErrorFrameExt.data` 的长度也按 `objectSize - calculateObjectSize()` 计算。

## 6. Rust 核心结构

建议目录结构：

```text
vector_log/
  Cargo.toml
  pyproject.toml
  crates/
    blf-core/
      src/
        lib.rs
        file.rs
        file_statistics.rs
        object_header.rs
        log_container.rs
        restore_point.rs
        objects/
          mod.rs
          can.rs
          lin.rs
          ethernet.rs
          app_text.rs
          raw.rs
      tests/
    blf-py/
      src/
        lib.rs
        file.rs
        objects.rs
        error.rs
  python/
    vector_log/
      blf_rs/
        __init__.py
        cantools_integration.py
  tests/
  benches/
  scripts/
```

`blf-core` 不依赖 Python。`blf-py` 只依赖 PyO3 和 `blf-core`。

### 6.1 Rust API

```rust
pub struct File {
    pub file_statistics: FileStatistics,
    pub current_uncompressed_file_size: u64,
    pub current_object_count: u32,
    pub compression_level: i32,
    pub write_restore_points: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>, mode: OpenMode) -> Result<Self, BlfError>;
    pub fn is_open(&self) -> bool;
    pub fn good(&self) -> bool;
    pub fn eof(&self) -> bool;
    pub fn read(&mut self) -> Result<Option<Object>, BlfError>;
    pub fn close(&mut self);
    pub fn read_batch(&mut self, max_objects: usize) -> Result<Vec<Object>, BlfError>;
}
```

`Object` 是一个类型化的枚举，Python 侧映射为对应的具体类。未知对象默认跳过，严格模式返回错误。

C++ 的 `File` 使用 `CompressedFile`、`UncompressedFile`、`ObjectQueue` 和后台线程实现生产者/消费者。这个线程模型不是 BLF 文件格式的一部分。Rust 可以简化为同步迭代器或有限大小的解压队列，但需要保持公开行为一致：

- `read()` 按逻辑流顺序返回对象。
- `LogContainer` 在内部消化。
- 未知对象跳过。
- `Unknown115` 作为 `RestorePointContainer` 返回。

## 7. Python API

### 7.1 与 C++ File 对齐的入口

```python
from vector_log.blf_rs import File, ObjectType

f = File()
f.open("trace.blf", "r")

print(f.fileStatistics.statisticsSize)
print(f.fileStatistics.objectCount)

while f.good():
    obj = f.read()
    if obj is None:
        break

    print(obj.objectType, obj.objectTimeStamp)

    if obj.objectType == ObjectType.CAN_MESSAGE2:
        print(obj.channel, obj.id, obj.data.hex())

f.close()
```

也可以使用上下文管理器：

```python
with File("trace.blf") as f:
    for obj in f.iter_objects():
        print(obj.objectType)
```

### 7.2 便捷接口

`File` 对齐 C++ 是主接口。为了测试脚本方便，可以额外提供：

```python
for can in f.can_messages():
    print(can.id, can.data)
```

`can_messages()` 只返回 `CanMessage`、`CanMessage2`、`CanFdMessage`、`CanFdMessage64`。

批量接口：

```python
objects = f.read_batch(4096)
```

### 7.3 字段名约定

Python 对象直接暴露 C++ 字段名：

```python
frame.objectTimeStamp
frame.objectFlags
frame.channel
frame.flags
frame.dlc
frame.id
frame.data
frame.frameLength
```

数据字段 `data` 使用不可变 `bytes`。大对象后续可以增加 `memoryview` 接口，但主接口先保持简单。

`ObjectHeader2` 对象额外暴露：

```python
frame.timeStampStatus
frame.reservedObjectHeader
frame.objectVersion
frame.objectTimeStamp
frame.originalTimeStamp
```

## 8. cantools 集成

核心解析不依赖 `cantools`。集成放在 Python 适配层：

```python
import cantools.database
from vector_log.blf_rs import File

db = cantools.database.load_file("network.dbc")

with File("trace.blf") as f:
    for frame in f.can_messages():
        decoded = db.decode_message(
            frame.id,
            frame.data,
            decode_choices=False,
        )
        print(frame.id, decoded)
```

可选辅助函数：

```python
from vector_log.blf_rs.cantools_integration import iter_decoded

for frame_id, decoded in iter_decoded("trace.blf", db):
    print(frame_id, decoded)
```

`cantools_integration` 只负责桥接，不重新实现 DBC 解码。

## 9. 对象覆盖优先级

### P0：必须支持

- `FileStatistics`
- `LogContainer`
- `RestorePointContainer` 和 `RestorePoint`，`File.read()` 默认返回 `RestorePointContainer`
- `ObjectHeaderBase`、`ObjectHeader`、`ObjectHeader2`、`VarObjectHeader`
- `CanMessage`、`CanMessage2`
- `CanFdMessage`、`CanFdMessage64`
- `CanErrorFrame`、`CanErrorFrameExt`、`CanFdErrorFrame64`
- `AppText`
- 未支持对象按 `objectSize` 跳过

### P1：第二优先级

- `CanOverloadFrame`
- `CanDriverStatistic`
- `CanDriverError`、`CanDriverErrorExt`、`CanDriverHwSync`
- `EnvironmentVariable`
- `AppTrigger`
- LIN 系列
- `EthernetFrame`、`EthernetFrameEx`
- `GpsEvent`

### P2：按需扩展

- FlexRay、MOST、J1708、A429、AFDX、WLAN、诊断对象
- 写 BLF 能力
- NumPy、Arrow 批量导出

## 10. 解析流程与性能

解析流程：

1. 打开文件，mmap 或流式读取。
2. 解析 `LOGG` 文件统计头。
3. 记录 `restorePointsOffset`，但按 `vector_blf` 当前读路径继续顺序处理所有外层容器。
4. 读取外层 `LogContainer`。
5. 按 `compressionMethod` 解压或直接读取。
6. 把解压内容接入连续逻辑流。
7. 扫描 `LOBJ` 签名并解析对象。
8. 根据 `objectType` 创建具体 Rust 类型；未知对象按 `objectSize` 跳过。
9. PyO3 把对象转换为对应 Python 类。

性能策略：

- mmap 减少系统调用。
- 只解压当前容器，不整文件加载。
- 复用解压缓冲区。
- 解析在 `&[u8]` 上做边界检查，不重复分配。
- 可选并行解压容器，通过序号重排保持事件顺序。
- 批量读取时释放 GIL。
- 经典 CAN 数据仅 8 字节，CAN FD 最多 64 字节，复制为 `bytes` 成本可接受。

## 11. 错误与安全

错误模型：

- `BlfError`
- `BlfFormatError`
- `BlfTruncatedError`
- `BlfUnsupportedVersionError`
- `BlfUnsupportedObjectError`
- `BlfCompressionError`
- `BlfLimitError`

安全要求：

- 所有长度、偏移、对象大小均做溢出和越界检查。
- 解压大小受上限约束。
- `LOBJ` 签名扫描不能无限循环，必须限制跳过范围。
- 未知对象按 `objectSize` 安全跳过。
- 严格模式用于开发测试，非严格模式用于兼容现场日志。

严格与非严格模式必须分别定义：

| 场景 | 默认/非严格 | 严格 |
| --- | --- | --- |
| 未知 `objectType` | 按 `objectSize` 跳过 | 抛 `BlfUnsupportedObjectError` |
| 已知但未实现的 P1/P2 类型 | 按 `objectSize` 跳过 | 可作为未实现告警 |
| 对象或容器截断 | 第一版默认抛 `BlfTruncatedError`，可按选项允许安全停止 | 抛 `BlfTruncatedError` |
| `statisticsSize != 144` | 读取并记录，不硬失败 | 可告警，仍不改变读取布局 |
| `LOBJ` 扫描超过上限 | 抛 `BlfFormatError` | 抛 `BlfFormatError` |

## 12. 测试策略

1. Rust 单元测试：文件头、对象头、容器、restore point、CAN 对象。
2. 结构对齐测试：逐字段验证 Rust 结构与 C++ 字段顺序一致。
3. Golden 测试：固定 BLF 样本验证对象数量、类型和字段值。
4. 跨容器测试：对象跨越 `LogContainer` 边界。
5. 损坏文件测试：截断容器、错误 `objectSize`、错误 zlib、未知对象。
6. Fuzz 测试：对解析入口输入随机字节。
7. Python 测试：类名、字段名、迭代、批量、异常映射。
8. cantools 集成测试：使用 DBC 解码已解析 CAN 数据。
9. Benchmark：区分 Rust 原始解析、逐事件 Python、批量 Python。

许可注意：`vector_blf` 测试文件是 GPL。仓库默认不内置这些文件，可提供脚本在开发环境自行下载做对照测试，不打包进发布产物。

## 13. 里程碑

### M0：格式与原型验证，约 0.5 周

- 收集真实 BLF 样本。
- 验证 `ObjectHeaderBase` 的 `LOBJ` 扫描和 `LogContainer` padding。
- 验证 `ablf`、`autors-blf` 是否适合作为 clean-room 原型参考。

### M1：Rust 核心 P0，约 1 周

- 实现 `FileStatistics`、`ObjectHeader`、`ObjectHeader2`、`LogContainer`。
- 实现 `File` 只读流程。
- 实现 P0 CAN 对象和 `AppText`。
- 完成结构对齐和损坏文件测试。

### M2：Python 接口，约 1 周

- 实现 PyO3 类，类名和字段名与 `vector_blf` 对齐。
- 实现 `File`、`read_batch`、上下文管理。
- maturin 构建 wheel。
- 完成 cantools 联调。

### M3：性能优化，约 1 周

- 建立 benchmark。
- 对比解压后端。
- 实现缓冲区复用和可选并行解压。

### M4：覆盖扩展与发布，约 1 到 2 周

- 扩展 P1 对象。
- 完善 fuzz、CI、跨平台 wheel。
- 生成文档和示例。

## 14. 验收标准

- Python 暴露的类名和字段名与 `vector_blf` 对应 C++ 类一致。
- P0 对象解析结果与 `vector_blf` 参考结果一致。
- `File.read()` 行为与 C++ `File::read()` 对齐，未知对象默认跳过。
- 大文件解析不整文件驻留内存。
- wheel 可在目标平台直接安装和 import。
- `can_messages()` 可直接用于 `cantools` DBC 解码。
- CI 覆盖 Rust 测试、Python 测试、clippy、fmt、fuzz smoke test 和 benchmark。

## 15. 风险与待确认

- `vector_blf` 是 GPL，必须做 clean-room 或只做行为对照，提交前需要合规确认。
- BLF 不同 API 版本可能存在字段差异，需要真实样本回归。
- zlib 解压可能成为性能瓶颈，需要尽早基准。
- Python 对象逐条转换可能限制极端吞吐，批量接口作为补充。
- 大端平台当前优先支持 little-endian，遇到时明确报错。

## 16. Deep-Interview 复核问题清单

以下是使用 `omx exec` 调用 `$deep-interview --quick`，对照本地 `vector_blf` 源码后记录的问题。实现时按 P1 优先处理，P2/P3 延后。

### P1：高优先级

1. `statisticsSize == 144` 不应作为硬校验。`vector_blf` 固定读取 144 字节布局，不校验该字段。
2. `CanFdMessage64.data` 和 `CanFdErrorFrame64.data` 的长度由 `validDataBytes` 决定，尾部才是 `reservedCanFdExtFrameData`。
3. `CanFdErrorFrame64.reservedCanFdErrorFrame64` 是死字段，二进制流中的尾部字节应映射到 `reservedCanFdExtFrameData`。

### P2：中优先级

4. 未知对象跳过与截断错误是两个独立维度，严格/非严格模式需要明确映射。
5. 对象分发按 `objectType`，不能只按 `headerVersion`；`LogContainer` 是 `ObjectHeaderBase` 特例。
6. `RestorePoints` 数量推导公式为 `(objectSize - 8) / 24`。
7. `apiNumber` 编码公式为 `major*1_000_000 + minor*1_000 + build*100 + patch`。
8. `LOBJ` 有界扫描是安全增强，与 C++ 扫到 EOF 不同，需要标注为有意差异。

### P3：低优先级

9. LIN 混合基类清单需逐类补齐，文档当前只列出一部分。
10. MOST 头类型边界是 `MostPkt` 到 `MostPkt2`，不能用“后续 MOST”笼统概括。
11. `good()`/`eof()` 在 C++ 中由 `ObjectQueue` 驱动，可靠 EOF 判断应使用 `read() is None`。
12. 已知但未实现的 P1/P2 类型，非严格模式下也必须按 `objectSize` 跳过。

## 17. 参考资料

- [Technica-Engineering/vector_blf](https://github.com/Technica-Engineering/vector_blf)
- [cantools/cantools](https://github.com/cantools/cantools)
- [mbehr1/ablf](https://github.com/mbehr1/ablf)
- [H2O2-IO/autors](https://github.com/H2O2-IO/autors)
