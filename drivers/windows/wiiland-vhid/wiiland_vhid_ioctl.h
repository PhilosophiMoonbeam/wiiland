#ifndef WIILAND_VHID_IOCTL_H
#define WIILAND_VHID_IOCTL_H

#include <stdint.h>
#include <winioctl.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WIILAND_VHID_ABI_VERSION 2u
#define WIILAND_VHID_REPORT_LAYOUT_VERSION 2u
#define WIILAND_VHID_MAX_SLOTS 32u
#define WIILAND_VHID_MAX_REPORT_PAYLOAD 88u

#define WIILAND_VHID_REPORT_GAMEPAD 1u
#define WIILAND_VHID_REPORT_SUPPLEMENTAL_AXES 2u

#define WIILAND_VHID_GAMEPAD_PAYLOAD_SIZE 17u
#define WIILAND_VHID_SUPPLEMENTAL_AXES_COUNT 22u
#define WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_SIZE 88u

/* Axis[] is ordered by core axis code: 0,1,3,4,2,5,22,23,40,24,25,26,27,
 * 6,7,8,9,10,16,18,19,20. Each element is signed 32-bit; indices 4,5,7,8
 * carry 0..1023, indices 9..12 carry 0..65535, and the remainder carry
 * -32768..32767. The driver validates these per-code ranges.
 */

/* Open the device interface for write access. Only the broker service SID,
 * LocalSystem, and local administrators are granted access by the INF ACL.
 */
#define WIILAND_VHID_IOCTL_CREATE \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_ACCESS)
#define WIILAND_VHID_IOCTL_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_WRITE_ACCESS)
#define WIILAND_VHID_IOCTL_DESTROY \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* Initialize a GUID with WIILAND_VHID_INTERFACE_GUID_INITIALIZER. */
#define WIILAND_VHID_INTERFACE_GUID_INITIALIZER \
    { 0x47576e0f, 0x990a, 0x4c5e, { 0xbd, 0xbd, 0xba, 0xda, 0x13, 0xaf, 0x75, 0x42 } }

#pragma pack(push, 1)

typedef struct WIILAND_VHID_CREATE_V2 {
    uint32_t Size;
    uint32_t Version;
    uint32_t Slot;
    uint32_t Flags; /* Must be zero. */
} WIILAND_VHID_CREATE_V2;

typedef struct WIILAND_VHID_CREATE_RESULT_V2 {
    uint32_t Size;
    uint32_t Version;
    uint32_t Slot;
    uint32_t Generation;
    uint32_t ReportLayoutVersion;
} WIILAND_VHID_CREATE_RESULT_V2;

typedef struct WIILAND_VHID_DESTROY_V2 {
    uint32_t Size;
    uint32_t Version;
    uint32_t Slot;
    uint32_t Generation;
} WIILAND_VHID_DESTROY_V2;

/* Gamepad payload excludes the report-ID byte. Buttons bits 0..27 map to
 * CORE_KEYS order and usages 1..28; the high nibble of Buttons is zero.
 * HatAndPadding uses the low nibble for 0..7 directions or 8 (neutral/null)
 * and requires the high nibble to be zero. X/Y/Rx/Ry map core axes
 * 0/1/3/4 (-32768..32767); Left/RightTrigger map axes 2/5 (0..1023).
 */
typedef struct WIILAND_VHID_GAMEPAD_PAYLOAD_V2 {
    uint32_t Buttons;
    uint8_t HatAndPadding;
    int16_t X;
    int16_t Y;
    int16_t Rx;
    int16_t Ry;
    uint16_t LeftTrigger;
    uint16_t RightTrigger;
} WIILAND_VHID_GAMEPAD_PAYLOAD_V2;

typedef struct WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_V2 {
    int32_t Axis[WIILAND_VHID_SUPPLEMENTAL_AXES_COUNT];
} WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_V2;

/* Payload is an exact report body without the report-ID byte. Unused payload
 * bytes and Reserved must be zero. The service supplies the active generation
 * returned by CREATE for every REPORT and DESTROY.
 */
typedef struct WIILAND_VHID_REPORT_V2 {
    uint32_t Size;
    uint32_t Version;
    uint32_t Slot;
    uint32_t Generation;
    uint8_t ReportId;
    uint8_t PayloadLength;
    uint8_t Reserved[2];
    uint8_t Payload[WIILAND_VHID_MAX_REPORT_PAYLOAD];
} WIILAND_VHID_REPORT_V2;

#pragma pack(pop)

#if defined(__cplusplus)
static_assert(sizeof(WIILAND_VHID_CREATE_V2) == 16, "CREATE ABI size");
static_assert(sizeof(WIILAND_VHID_CREATE_RESULT_V2) == 20, "CREATE result ABI size");
static_assert(sizeof(WIILAND_VHID_DESTROY_V2) == 16, "DESTROY ABI size");
static_assert(sizeof(WIILAND_VHID_GAMEPAD_PAYLOAD_V2) == WIILAND_VHID_GAMEPAD_PAYLOAD_SIZE, "gamepad report size");
static_assert(sizeof(WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_V2) == WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_SIZE, "axes report size");
static_assert(sizeof(WIILAND_VHID_REPORT_V2) == 108, "REPORT ABI size");
#else
typedef char WIILAND_VHID_ASSERT_CREATE[(sizeof(WIILAND_VHID_CREATE_V2) == 16) ? 1 : -1];
typedef char WIILAND_VHID_ASSERT_CREATE_RESULT[(sizeof(WIILAND_VHID_CREATE_RESULT_V2) == 20) ? 1 : -1];
typedef char WIILAND_VHID_ASSERT_DESTROY[(sizeof(WIILAND_VHID_DESTROY_V2) == 16) ? 1 : -1];
typedef char WIILAND_VHID_ASSERT_GAMEPAD[(sizeof(WIILAND_VHID_GAMEPAD_PAYLOAD_V2) == WIILAND_VHID_GAMEPAD_PAYLOAD_SIZE) ? 1 : -1];
typedef char WIILAND_VHID_ASSERT_AXES[(sizeof(WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_V2) == WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_SIZE) ? 1 : -1];
typedef char WIILAND_VHID_ASSERT_REPORT[(sizeof(WIILAND_VHID_REPORT_V2) == 108) ? 1 : -1];
#endif

#ifdef __cplusplus
}
#endif

#endif /* WIILAND_VHID_IOCTL_H */
