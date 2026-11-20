# BLF object support matrix

Which object types this reader understands, and which of them produce a
decoded row in the message list. Regenerate with:

```sh
python3 tools/gen-support-matrix.py
```

| id | type | bus | fields parsed | list row |
|---:|---|---|---|---|
| 1 | `CanMessage` | General | yes | yes |
| 2 | `CanErrorFrame` | General | yes | yes |
| 3 | `CanOverloadFrame` | General | yes | yes |
| 4 | `CanDriverStatistic` | General | yes | yes |
| 5 | `AppTrigger` | General | yes | yes |
| 6 | `EnvironmentVariable` | General | yes | yes |
| 7 | `EnvironmentVariable` | General | yes | yes |
| 8 | `EnvironmentVariable` | General | yes | yes |
| 9 | `EnvironmentVariable` | General | yes | yes |
| 11 | `LinMessage` | General | yes | yes |
| 12 | `LinCrcError` | General | yes | yes |
| 13 | `LinDlcInfo` | General | yes | yes |
| 14 | `LinReceiveError` | General | yes | yes |
| 15 | `LinSendError` | General | yes | yes |
| 16 | `LinSlaveTimeout` | General | yes | yes |
| 17 | `LinSchedulerModeChange` | General | yes | yes |
| 18 | `LinSyncError` | General | yes | yes |
| 19 | `LinBaudrateEvent` | General | yes | yes |
| 20 | `LinSleepModeEvent` | General | yes | yes |
| 21 | `LinWakeupEvent` | General | yes | yes |
| 22 | `MostSpy` | General | yes | yes |
| 23 | `MostCtrl` | General | yes | yes |
| 24 | `MostLightLock` | General | yes | yes |
| 25 | `MostStatistic` | General | yes | yes |
| 29 | `FlexRayData` | General | yes | yes |
| 30 | `FlexRaySync` | General | yes | yes |
| 31 | `CanDriverError` | General | yes | yes |
| 32 | `MostPkt` | General | yes | yes |
| 33 | `MostPkt2` | General | yes | yes |
| 34 | `MostHwMode` | General | yes | yes |
| 35 | `MostReg` | General | yes | yes |
| 36 | `MostGenReg` | General | yes | yes |
| 37 | `MostNetState` | General | yes | yes |
| 38 | `MostDataLost` | General | yes | yes |
| 39 | `MostTrigger` | General | yes | yes |
| 40 | `FlexRayV6StartCycleEvent` | General | yes | yes |
| 41 | `FlexRayV6Message` | General | yes | yes |
| 42 | `LinChecksumInfo` | General | yes | yes |
| 43 | `LinSpikeEvent` | General | yes | yes |
| 44 | `CanDriverHwSync` | General | yes | yes |
| 45 | `FlexRayStatusEvent` | General | yes | yes |
| 46 | `GpsEvent` | General | yes | yes |
| 47 | `FlexRayVFrError` | General | yes | yes |
| 48 | `FlexRayVFrStatus` | General | yes | yes |
| 49 | `FlexRayVFrStartCycle` | General | yes | yes |
| 50 | `FlexRayVFrReceiveMsg` | General | yes | yes |
| 51 | `RealtimeClock` | General | yes | yes |
| 54 | `LinStatisticEvent` | General | yes | yes |
| 55 | `J1708Message` | J1708 | yes | yes |
| 56 | `J1708Message` | J1708 | yes | yes |
| 57 | `LinMessage2` | General | yes | yes |
| 58 | `LinSendError2` | General | yes | yes |
| 59 | `LinSyncError2` | General | yes | yes |
| 60 | `LinCrcError2` | General | yes | yes |
| 61 | `LinReceiveError2` | General | yes | yes |
| 62 | `LinWakeupEvent2` | General | yes | yes |
| 63 | `LinSpikeEvent2` | General | yes | yes |
| 64 | `LinLongDomSignalEvent` | General | yes | yes |
| 65 | `AppText` | General | yes | yes |
| 66 | `FlexRayVFrReceiveMsgEx` | General | yes | yes |
| 67 | `MostStatisticEx` | General | yes | yes |
| 68 | `MostTxLight` | General | yes | yes |
| 69 | `MostAllocTab` | General | yes | yes |
| 70 | `MostStress` | General | yes | yes |
| 71 | `EthernetFrame` | General | yes | yes |
| 72 | `SystemVariable` | General | yes | yes |
| 73 | `CanErrorFrameExt` | General | yes | yes |
| 74 | `CanDriverErrorExt` | General | yes | yes |
| 75 | `LinLongDomSignalEvent2` | General | yes | yes |
| 76 | `Most150Message` | General | yes | yes |
| 77 | `Most150Pkt` | General | yes | yes |
| 78 | `MostEthernetPkt` | General | yes | yes |
| 79 | `Most150MessageFragment` | General | yes | yes |
| 80 | `Most150PktFragment` | General | yes | yes |
| 81 | `MostEthernetPktFragment` | General | yes | yes |
| 82 | `MostSystemEvent` | General | yes | yes |
| 83 | `Most150AllocTab` | General | yes | yes |
| 84 | `Most50Message` | General | yes | yes |
| 85 | `Most50Pkt` | General | yes | yes |
| 86 | `CanMessage2` | General | yes | yes |
| 87 | `LinUnexpectedWakeup` | General | yes | yes |
| 88 | `LinShortOrSlowResponse` | General | yes | yes |
| 89 | `LinDisturbanceEvent` | General | yes | yes |
| 90 | `SerialEvent` | General | yes | yes |
| 91 | `DriverOverrun` | General | yes | yes |
| 92 | `EventComment` | General | yes | yes |
| 93 | `WlanFrame` | General | yes | yes |
| 94 | `WlanStatistic` | General | yes | yes |
| 95 | `MostEcl` | General | yes | yes |
| 96 | `GlobalMarker` | General | yes | yes |
| 97 | `AfdxFrame` | General | yes | yes |
| 98 | `AfdxStatistic` | General | yes | yes |
| 99 | `KLineStatusEvent` | General | yes | yes |
| 100 | `CanFdMessage` | General | yes | yes |
| 101 | `CanFdMessage64` | General | yes | yes |
| 102 | `EthernetRxError` | General | yes | yes |
| 103 | `EthernetStatus` | General | yes | yes |
| 104 | `CanFdErrorFrame64` | General | yes | yes |
| 105 | `LinShortOrSlowResponse2` | General | yes | yes |
| 106 | `AfdxStatus` | General | yes | yes |
| 107 | `AfdxBusStatistic` | General | yes | yes |
| 109 | `AfdxErrorEvent` | General | yes | yes |
| 110 | `A429Error` | A429 | yes | yes |
| 111 | `A429Status` | A429 | yes | yes |
| 112 | `A429BusStatistic` | A429 | yes | yes |
| 113 | `A429Message` | A429 | yes | yes |
| 114 | `EthernetStatistic` | General | yes | yes |
| 118 | `TestStructure` | General | yes | yes |
| 119 | `DiagRequestInterpretation` | General | yes | yes |
| 120 | `EthernetFrameEx` | General | yes | yes |
| 121 | `EthernetFrameForwarded` | General | yes | yes |
| 122 | `EthernetErrorEx` | General | yes | yes |
| 123 | `EthernetErrorForwarded` | General | yes | yes |
| 124 | `FunctionBus` | General | yes | yes |
| 125 | `DataLostBegin` | General | yes | yes |
| 126 | `DataLostEnd` | General | yes | yes |
| 127 | `WaterMarkEvent` | General | yes | yes |
| 128 | `TriggerCondition` | General | yes | yes |
| 129 | `CanSettingChanged` | General | yes | yes |
| 130 | `DistributedObjectMember` | General | yes | yes |
| 131 | `AttributeEvent` | General | yes | yes |
| 132 | `DistributedObjectChange` | General | yes | yes |
| 133 | `EthernetPhyState` | General | yes | yes |

123 object types: 123 with parsed fields, 123 with a decoded list row.
