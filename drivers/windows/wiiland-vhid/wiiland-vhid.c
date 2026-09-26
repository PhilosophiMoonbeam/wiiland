#include <ntddk.h>
#include <wdf.h>
#include <hidport.h>
#include <hidclass.h>
#include <vhf.h>

#include "wiiland_vhid_ioctl.h"

#define WIILAND_VHID_TEST_VENDOR_ID 0xFFFFu
#define WIILAND_VHID_PRODUCT_ID 0x0001u
#define WIILAND_VHID_DEVICE_VERSION 0x0200u

typedef struct _WIILAND_VHID_SLOT {
    VHFHANDLE Handle;
    WDFFILEOBJECT Owner;
    ULONG Generation;
    ULONG LastGeneration;
    BOOLEAN Active;
} WIILAND_VHID_SLOT, *PWIILAND_VHID_SLOT;

typedef struct _WIILAND_VHID_DEVICE_CONTEXT {
    WDFWAITLOCK SlotLock;
    WIILAND_VHID_SLOT Slots[WIILAND_VHID_MAX_SLOTS];
} WIILAND_VHID_DEVICE_CONTEXT, *PWIILAND_VHID_DEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(WIILAND_VHID_DEVICE_CONTEXT, WiiLandGetDeviceContext)

typedef struct _WIILAND_VHID_FILE_CONTEXT {
    WDFDEVICE Device;
} WIILAND_VHID_FILE_CONTEXT, *PWIILAND_VHID_FILE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(WIILAND_VHID_FILE_CONTEXT, WiiLandGetFileContext)

EVT_WDF_DRIVER_DEVICE_ADD WiiLandEvtDriverDeviceAdd;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL WiiLandEvtIoDeviceControl;
EVT_WDF_FILE_CREATE WiiLandEvtFileCreate;
EVT_WDF_FILE_CLEANUP WiiLandEvtFileCleanup;
EVT_WDF_OBJECT_CONTEXT_CLEANUP WiiLandEvtDeviceCleanup;

static const GUID g_WiiLandVhidInterfaceGuid = WIILAND_VHID_INTERFACE_GUID_INITIALIZER;

/* Each Application collection is a separate HID top-level collection. Report
 * bodies are fixed; the report ID is provided separately to VHF and by the ABI.
 */
static const UCHAR g_WiiLandReportDescriptor[] = {
    /* Standard gamepad TLC: all 28 core buttons, one hat, four signed stick
     * axes, and two unsigned 16-bit triggers with logical range 0..1023. */
    0x05, 0x01,             /* USAGE_PAGE (Generic Desktop) */
    0x09, 0x05,             /* USAGE (Game Pad) */
    0xA1, 0x01,             /* COLLECTION (Application) */
    0x85, WIILAND_VHID_REPORT_GAMEPAD,
    0x05, 0x09,             /* USAGE_PAGE (Button) */
    0x19, 0x01,             /* USAGE_MINIMUM (Button 1) */
    0x29, 0x1C,             /* USAGE_MAXIMUM (Button 28) */
    0x15, 0x00,             /* LOGICAL_MINIMUM (0) */
    0x25, 0x01,             /* LOGICAL_MAXIMUM (1) */
    0x75, 0x01,             /* REPORT_SIZE (1) */
    0x95, 0x1C,             /* REPORT_COUNT (28) */
    0x81, 0x02,             /* INPUT (Data, Variable, Absolute) */
    0x75, 0x04,             /* REPORT_SIZE (4) */
    0x95, 0x01,             /* REPORT_COUNT (1) */
    0x81, 0x01,             /* INPUT (Constant, Array, Absolute) */
    0x05, 0x01,             /* USAGE_PAGE (Generic Desktop) */
    0x09, 0x39,             /* USAGE (Hat Switch) */
    0x15, 0x00,             /* LOGICAL_MINIMUM (0) */
    0x25, 0x07,             /* LOGICAL_MAXIMUM (7; 8 is null/neutral) */
    0x75, 0x04,             /* REPORT_SIZE (4) */
    0x95, 0x01,             /* REPORT_COUNT (1) */
    0x81, 0x42,             /* INPUT (Data, Variable, Absolute, Null State) */
    0x75, 0x04,             /* REPORT_SIZE (4) */
    0x95, 0x01,             /* REPORT_COUNT (1) */
    0x81, 0x01,             /* INPUT (Constant, Array, Absolute) */
    0x09, 0x30,             /* USAGE (X) */
    0x09, 0x31,             /* USAGE (Y) */
    0x09, 0x33,             /* USAGE (Rx) */
    0x09, 0x34,             /* USAGE (Ry) */
    0x16, 0x00, 0x80,       /* LOGICAL_MINIMUM (-32768) */
    0x26, 0xFF, 0x7F,       /* LOGICAL_MAXIMUM (32767) */
    0x75, 0x10,             /* REPORT_SIZE (16) */
    0x95, 0x04,             /* REPORT_COUNT (4) */
    0x81, 0x02,             /* INPUT (Data, Variable, Absolute) */
    0x09, 0x32,             /* USAGE (Z; core axis code 2, left trigger) */
    0x09, 0x35,             /* USAGE (Rz; core axis code 5, right trigger) */
    0x15, 0x00,             /* LOGICAL_MINIMUM (0) */
    0x26, 0xFF, 0x03,       /* LOGICAL_MAXIMUM (1023) */
    0x75, 0x10,             /* REPORT_SIZE (16) */
    0x95, 0x02,             /* REPORT_COUNT (2) */
    0x81, 0x02,             /* INPUT (Data, Variable, Absolute) */
    0xC0,                   /* END_COLLECTION */

    /* Supplemental joystick TLC. Axis[] order follows core axis codes
     * [0,1,3,4,2,5,22,23,40,24,25,26,27,6,7,8,9,10,16,18,19,20].
     * The first nine use standard Generic Desktop axes X..Wheel. The last
     * thirteen use stable WiiLand vendor usages 1..13 on page 0xFF00.
     * Every payload element is 32-bit; logical bounds mirror axis_info.
     */
    0x05, 0x01,             /* USAGE_PAGE (Generic Desktop) */
    0x09, 0x04,             /* USAGE (Joystick) */
    0xA1, 0x01,             /* COLLECTION (Application) */
    0x85, WIILAND_VHID_REPORT_SUPPLEMENTAL_AXES,
    0x75, 0x20,             /* REPORT_SIZE (32) */
    0x95, 0x01,             /* REPORT_COUNT (1) */
    0x09, 0x30,             /* core axis code 0: X */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x31,             /* core axis code 1: Y */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x33,             /* core axis code 3: Rx */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x34,             /* core axis code 4: Ry */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x32,             /* core axis code 2: Z, 0..1023 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0x03, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x35,             /* core axis code 5: Rz, 0..1023 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0x03, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x36,             /* core axis code 22: Slider */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x37,             /* core axis code 23: Dial, 0..1023 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0x03, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x38,             /* core axis code 40: Wheel, 0..1023 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0x03, 0x00, 0x00,
    0x81, 0x02,
    0x06, 0x00, 0xFF,       /* USAGE_PAGE (WiiLand axes 0xFF00) */
    0x09, 0x01,             /* core axis code 24: vendor usage 1, 0..65535 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0xFF, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x02,             /* core axis code 25: vendor usage 2, 0..65535 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0xFF, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x03,             /* core axis code 26: vendor usage 3, 0..65535 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0xFF, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x04,             /* core axis code 27: vendor usage 4, 0..65535 */
    0x17, 0x00, 0x00, 0x00, 0x00,
    0x27, 0xFF, 0xFF, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x05,             /* core axis code 6: vendor usage 5 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x06,             /* core axis code 7: vendor usage 6 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x07,             /* core axis code 8: vendor usage 7 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x08,             /* core axis code 9: vendor usage 8 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x09,             /* core axis code 10: vendor usage 9 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x0A,             /* core axis code 16: vendor usage 10 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x0B,             /* core axis code 18: vendor usage 11 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x0C,             /* core axis code 19: vendor usage 12 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0x09, 0x0D,             /* core axis code 20: vendor usage 13 */
    0x17, 0x00, 0x80, 0xFF, 0xFF,
    0x27, 0xFF, 0x7F, 0x00, 0x00,
    0x81, 0x02,
    0xC0                    /* END_COLLECTION */

};

static ULONG
WiiLandNextGeneration(
    _In_ ULONG Previous
    )
{
    ULONG next = Previous + 1;
    return (next == 0) ? 1 : next;
}

static ULONG
WiiLandExpectedPayloadLength(
    _In_ UCHAR ReportId
    )
{
    switch (ReportId) {
    case WIILAND_VHID_REPORT_GAMEPAD:
        return WIILAND_VHID_GAMEPAD_PAYLOAD_SIZE;
    case WIILAND_VHID_REPORT_SUPPLEMENTAL_AXES:
        return WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_SIZE;
    default:
        return 0;
    }
}

static BOOLEAN
WiiLandAxisValueIsValid(
    _In_ ULONG Index,
    _In_ LONG Value
    )
{
    if (Index == 4 || Index == 5 || Index == 7 || Index == 8) {
        return Value >= 0 && Value <= 1023;
    }
    if (Index >= 9 && Index <= 12) {
        return Value >= 0 && Value <= 65535;
    }
    return Value >= -32768 && Value <= 32767;
}

static BOOLEAN
WiiLandValidateReport(
    _In_ const WIILAND_VHID_REPORT_V2 *Report
    )
{
    ULONG expected = WiiLandExpectedPayloadLength(Report->ReportId);
    ULONG index;

    if (expected == 0 || Report->PayloadLength != expected ||
        Report->Reserved[0] != 0 || Report->Reserved[1] != 0) {
        return FALSE;
    }

    for (index = expected; index < WIILAND_VHID_MAX_REPORT_PAYLOAD; ++index) {
        if (Report->Payload[index] != 0) {
            return FALSE;
        }
    }

    if (Report->ReportId == WIILAND_VHID_REPORT_GAMEPAD) {
        ULONG leftTrigger;
        ULONG rightTrigger;

        if ((Report->Payload[3] & 0xF0) != 0 ||
            (Report->Payload[4] & 0xF0) != 0 ||
            (Report->Payload[4] & 0x0F) > 8) {
            return FALSE;
        }
        leftTrigger = Report->Payload[13] |
            ((ULONG)Report->Payload[14] << 8);
        rightTrigger = Report->Payload[15] |
            ((ULONG)Report->Payload[16] << 8);
        if (leftTrigger > 1023 || rightTrigger > 1023) {
            return FALSE;
        }
    } else if (Report->ReportId == WIILAND_VHID_REPORT_SUPPLEMENTAL_AXES) {
        for (index = 0; index < WIILAND_VHID_SUPPLEMENTAL_AXES_COUNT; ++index) {
            LONG value;
            RtlCopyMemory(&value, &Report->Payload[index * sizeof(value)], sizeof(value));
            if (!WiiLandAxisValueIsValid(index, value)) {
                return FALSE;
            }
        }
    }
    return TRUE;
}

static NTSTATUS
WiiLandSubmitReport(
    _In_ VHFHANDLE Handle,
    _In_ UCHAR ReportId,
    _In_reads_bytes_(PayloadLength) const UCHAR *Payload,
    _In_ ULONG PayloadLength
    )
{
    UCHAR hidReport[WIILAND_VHID_MAX_REPORT_PAYLOAD + 1];
    HID_XFER_PACKET packet;

    /* HID_XFER_PACKET.reportBuffer is the full HID report, including its
     * leading report-ID byte; the public IOCTL ABI carries the body only.
     */
    hidReport[0] = ReportId;
    RtlCopyMemory(&hidReport[1], Payload, PayloadLength);
    packet.reportBuffer = hidReport;
    packet.reportBufferLen = PayloadLength + 1;
    packet.reportId = ReportId;
    return VhfReadReportSubmit(Handle, &packet);
}

static VOID
WiiLandNeutralizeAndDeleteSlot(
    _Inout_ PWIILAND_VHID_SLOT Slot
    )
{
    static const UCHAR neutralGamepad[WIILAND_VHID_GAMEPAD_PAYLOAD_SIZE] = {
        0x00, 0x00, 0x00, 0x00, 0x08
    };
    static const UCHAR neutralAxes[WIILAND_VHID_SUPPLEMENTAL_AXES_PAYLOAD_SIZE] = { 0 };

    if (!Slot->Active || Slot->Handle == WDF_NO_HANDLE) {
        return;
    }

    /* The VHF default buffering policy accepts a caller-owned report until
     * VhfReadReportSubmit returns. Removal then tears down every HID TLC. */
    (VOID)WiiLandSubmitReport(Slot->Handle, WIILAND_VHID_REPORT_GAMEPAD,
        neutralGamepad, sizeof(neutralGamepad));
    (VOID)WiiLandSubmitReport(Slot->Handle, WIILAND_VHID_REPORT_SUPPLEMENTAL_AXES,
        neutralAxes, sizeof(neutralAxes));

    /* Synchronous deletion is PASSIVE_LEVEL-only; queue and device callbacks
     * are configured for PASSIVE_LEVEL. */
    VhfDelete(Slot->Handle, TRUE);
    Slot->Handle = WDF_NO_HANDLE;
    Slot->Owner = WDF_NO_HANDLE;
    Slot->Generation = 0;
    Slot->Active = FALSE;
}

static NTSTATUS
WiiLandCreateSlot(
    _In_ WDFDEVICE Device,
    _In_ WDFFILEOBJECT Owner,
    _In_ const WIILAND_VHID_CREATE_V2 *Input,
    _Out_ WIILAND_VHID_CREATE_RESULT_V2 *Output
    )
{
    PWIILAND_VHID_DEVICE_CONTEXT context = WiiLandGetDeviceContext(Device);
    PWIILAND_VHID_SLOT slot;
    WDFWAITLOCK lock = context->SlotLock;
    VHF_CONFIG config;
    VHFHANDLE handle = WDF_NO_HANDLE;
    ULONG generation;
    NTSTATUS status;

    if (Input->Size != sizeof(*Input) || Input->Version != WIILAND_VHID_ABI_VERSION ||
        Input->Slot >= WIILAND_VHID_MAX_SLOTS || Input->Flags != 0) {
        return STATUS_INVALID_PARAMETER;
    }

    status = WdfWaitLockAcquire(lock, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    slot = &context->Slots[Input->Slot];
    if (slot->Active) {
        WdfWaitLockRelease(lock);
        return STATUS_DEVICE_BUSY;
    }

    generation = WiiLandNextGeneration(slot->LastGeneration);
    slot->LastGeneration = generation;

    VHF_CONFIG_INIT(&config, WdfDeviceWdmGetDeviceObject(Device),
        sizeof(g_WiiLandReportDescriptor), (PUCHAR)g_WiiLandReportDescriptor);
    config.VendorID = WIILAND_VHID_TEST_VENDOR_ID;
    config.ProductID = WIILAND_VHID_PRODUCT_ID;
    config.VersionNumber = WIILAND_VHID_DEVICE_VERSION;

    status = VhfCreate(&config, &handle);
    if (NT_SUCCESS(status)) {
        status = VhfStart(handle);
        if (!NT_SUCCESS(status)) {
            VhfDelete(handle, TRUE);
            handle = WDF_NO_HANDLE;
        }
    }

    if (NT_SUCCESS(status)) {
        slot->Handle = handle;
        slot->Owner = Owner;
        slot->Generation = generation;
        slot->Active = TRUE;

        Output->Size = sizeof(*Output);
        Output->Version = WIILAND_VHID_ABI_VERSION;
        Output->Slot = Input->Slot;
        Output->Generation = generation;
        Output->ReportLayoutVersion = WIILAND_VHID_REPORT_LAYOUT_VERSION;
    }

    WdfWaitLockRelease(lock);
    return status;
}

static NTSTATUS
WiiLandReportToSlot(
    _In_ WDFDEVICE Device,
    _In_ WDFFILEOBJECT Owner,
    _In_ const WIILAND_VHID_REPORT_V2 *Input
    )
{
    PWIILAND_VHID_DEVICE_CONTEXT context = WiiLandGetDeviceContext(Device);
    PWIILAND_VHID_SLOT slot;
    UCHAR reportBytes[WIILAND_VHID_MAX_REPORT_PAYLOAD];
    NTSTATUS status;

    if (Input->Size != sizeof(*Input) || Input->Version != WIILAND_VHID_ABI_VERSION ||
        Input->Slot >= WIILAND_VHID_MAX_SLOTS || Input->Generation == 0 ||
        !WiiLandValidateReport(Input)) {
        return STATUS_INVALID_PARAMETER;
    }

    status = WdfWaitLockAcquire(context->SlotLock, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    slot = &context->Slots[Input->Slot];
    if (slot->Owner != Owner) {
        status = STATUS_ACCESS_DENIED;
    } else if (!slot->Active || slot->Handle == WDF_NO_HANDLE) {
        status = STATUS_DEVICE_NOT_READY;
    } else if (slot->Generation != Input->Generation) {
        status = STATUS_REVISION_MISMATCH;
    } else {
        RtlCopyMemory(reportBytes, Input->Payload, Input->PayloadLength);
        status = WiiLandSubmitReport(slot->Handle, Input->ReportId,
            reportBytes, Input->PayloadLength);
    }

    WdfWaitLockRelease(context->SlotLock);
    return status;
}

static NTSTATUS
WiiLandDestroySlot(
    _In_ WDFDEVICE Device,
    _In_ WDFFILEOBJECT Owner,
    _In_ const WIILAND_VHID_DESTROY_V2 *Input
    )
{
    PWIILAND_VHID_DEVICE_CONTEXT context = WiiLandGetDeviceContext(Device);
    PWIILAND_VHID_SLOT slot;
    NTSTATUS status;

    if (Input->Size != sizeof(*Input) || Input->Version != WIILAND_VHID_ABI_VERSION ||
        Input->Slot >= WIILAND_VHID_MAX_SLOTS || Input->Generation == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    status = WdfWaitLockAcquire(context->SlotLock, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    slot = &context->Slots[Input->Slot];
    if (slot->Owner != Owner) {
        status = STATUS_ACCESS_DENIED;
    } else if (!slot->Active) {
        status = STATUS_DEVICE_NOT_READY;
    } else if (slot->Generation != Input->Generation) {
        status = STATUS_REVISION_MISMATCH;
    } else {
        WiiLandNeutralizeAndDeleteSlot(slot);
        status = STATUS_SUCCESS;
    }

    WdfWaitLockRelease(context->SlotLock);
    return status;
}

VOID
WiiLandEvtFileCreate(
    _In_ WDFDEVICE Device,
    _In_ WDFREQUEST Request,
    _In_ WDFFILEOBJECT FileObject
    )
{
    PWIILAND_VHID_FILE_CONTEXT fileContext = WiiLandGetFileContext(FileObject);
    fileContext->Device = Device;
    WdfRequestComplete(Request, STATUS_SUCCESS);
}

VOID
WiiLandEvtFileCleanup(
    _In_ WDFFILEOBJECT FileObject
    )
{
    PWIILAND_VHID_FILE_CONTEXT fileContext = WiiLandGetFileContext(FileObject);
    PWIILAND_VHID_DEVICE_CONTEXT context = WiiLandGetDeviceContext(fileContext->Device);
    ULONG slotIndex;

    if (context->SlotLock == WDF_NO_HANDLE) {
        return;
    }

    if (NT_SUCCESS(WdfWaitLockAcquire(context->SlotLock, NULL))) {
        for (slotIndex = 0; slotIndex < WIILAND_VHID_MAX_SLOTS; ++slotIndex) {
            PWIILAND_VHID_SLOT slot = &context->Slots[slotIndex];
            if (slot->Active && slot->Owner == FileObject) {
                WiiLandNeutralizeAndDeleteSlot(slot);
            }
        }
        WdfWaitLockRelease(context->SlotLock);
    }
}

VOID
WiiLandEvtDeviceCleanup(
    _In_ WDFOBJECT Object
    )
{
    PWIILAND_VHID_DEVICE_CONTEXT context = WiiLandGetDeviceContext((WDFDEVICE)Object);
    ULONG slotIndex;

    if (context->SlotLock == WDF_NO_HANDLE) {
        return;
    }

    if (NT_SUCCESS(WdfWaitLockAcquire(context->SlotLock, NULL))) {
        for (slotIndex = 0; slotIndex < WIILAND_VHID_MAX_SLOTS; ++slotIndex) {
            WiiLandNeutralizeAndDeleteSlot(&context->Slots[slotIndex]);
        }
        WdfWaitLockRelease(context->SlotLock);
    }
}

VOID
WiiLandEvtIoDeviceControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode
    )
{
    WDFDEVICE device = WdfIoQueueGetDevice(Queue);
    WDFFILEOBJECT fileObject = WdfRequestGetFileObject(Request);
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR information = 0;

    if (fileObject == WDF_NO_HANDLE) {
        WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
        return;
    }

    switch (IoControlCode) {
    case WIILAND_VHID_IOCTL_CREATE:
    {
        WIILAND_VHID_CREATE_V2 input;
        WIILAND_VHID_CREATE_RESULT_V2 *output;
        PVOID inputBuffer;
        size_t actualInputLength = 0;

        if (InputBufferLength != sizeof(input)) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        if (OutputBufferLength < sizeof(*output)) {
            status = STATUS_BUFFER_TOO_SMALL;
            break;
        }
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(input),
            &inputBuffer, &actualInputLength);
        if (!NT_SUCCESS(status) || actualInputLength != sizeof(input)) {
            if (NT_SUCCESS(status)) {
                status = STATUS_INVALID_PARAMETER;
            }
            break;
        }
        RtlCopyMemory(&input, inputBuffer, sizeof(input));
        status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*output),
            (PVOID *)&output, NULL);
        if (!NT_SUCCESS(status)) {
            break;
        }
        status = WiiLandCreateSlot(device, fileObject, &input, output);
        if (NT_SUCCESS(status)) {
            information = sizeof(*output);
        }
        break;
    }
    case WIILAND_VHID_IOCTL_REPORT:
    {
        WIILAND_VHID_REPORT_V2 *input;
        size_t actualInputLength = 0;

        if (InputBufferLength != sizeof(*input) || OutputBufferLength != 0) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*input),
            (PVOID *)&input, &actualInputLength);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (actualInputLength != sizeof(*input)) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        status = WiiLandReportToSlot(device, fileObject, input);
        break;
    }
    case WIILAND_VHID_IOCTL_DESTROY:
    {
        WIILAND_VHID_DESTROY_V2 *input;
        size_t actualInputLength = 0;

        if (InputBufferLength != sizeof(*input) || OutputBufferLength != 0) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*input),
            (PVOID *)&input, &actualInputLength);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (actualInputLength != sizeof(*input)) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        status = WiiLandDestroySlot(device, fileObject, input);
        break;
    }
    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        break;
    }

    WdfRequestCompleteWithInformation(Request, status, information);
}

NTSTATUS
WiiLandEvtDriverDeviceAdd(
    _In_ WDFDRIVER Driver,
    _Inout_ PWDFDEVICE_INIT DeviceInit
    )
{
    WDF_OBJECT_ATTRIBUTES deviceAttributes;
    WDF_OBJECT_ATTRIBUTES fileAttributes;
    WDF_OBJECT_ATTRIBUTES lockAttributes;
    WDF_OBJECT_ATTRIBUTES queueAttributes;
    WDF_FILEOBJECT_CONFIG fileConfig;
    WDF_IO_QUEUE_CONFIG queueConfig;
    WDFDEVICE device;
    WDFWAITLOCK lock;
    PWIILAND_VHID_DEVICE_CONTEXT context;
    NTSTATUS status;
    UNREFERENCED_PARAMETER(Driver);

    WdfDeviceInitSetDeviceType(DeviceInit, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetCharacteristics(DeviceInit, FILE_DEVICE_SECURE_OPEN, FALSE);

    WDF_FILEOBJECT_CONFIG_INIT(&fileConfig, WiiLandEvtFileCreate, NULL,
        WiiLandEvtFileCleanup);
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttributes,
        WIILAND_VHID_FILE_CONTEXT);
    WdfDeviceInitSetFileObjectConfig(DeviceInit, &fileConfig, &fileAttributes);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&deviceAttributes,
        WIILAND_VHID_DEVICE_CONTEXT);
    deviceAttributes.EvtCleanupCallback = WiiLandEvtDeviceCleanup;
    deviceAttributes.ExecutionLevel = WdfExecutionLevelPassive;

    status = WdfDeviceCreate(&DeviceInit, &deviceAttributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    context = WiiLandGetDeviceContext(device);
    RtlZeroMemory(context, sizeof(*context));

    WDF_OBJECT_ATTRIBUTES_INIT(&lockAttributes);
    lockAttributes.ParentObject = device;
    status = WdfWaitLockCreate(&lockAttributes, &lock);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    context->SlotLock = lock;

    status = WdfDeviceCreateDeviceInterface(device,
        &g_WiiLandVhidInterfaceGuid, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig,
        WdfIoQueueDispatchSequential);
    queueConfig.EvtIoDeviceControl = WiiLandEvtIoDeviceControl;
    WDF_OBJECT_ATTRIBUTES_INIT(&queueAttributes);
    queueAttributes.ExecutionLevel = WdfExecutionLevelPassive;
    status = WdfIoQueueCreate(device, &queueConfig, &queueAttributes, NULL);
    return status;
}

NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PUNICODE_STRING RegistryPath
    )
{
    WDF_DRIVER_CONFIG config;

    WDF_DRIVER_CONFIG_INIT(&config, WiiLandEvtDriverDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath,
        WDF_NO_OBJECT_ATTRIBUTES, &config, NULL);
}
