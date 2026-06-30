MEMORY {
    BOOT2   : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH   : ORIGIN = 0x10000100, LENGTH = 2048K - 0x100
    RAM     : ORIGIN = 0x20000000, LENGTH = 255K
    PANDUMP : ORIGIN = 0x2003FC00, LENGTH = 1K
}

/* Símbolos requeridos por panic-persist para encontrar la zona */
_panic_dump_start = ORIGIN(PANDUMP);
_panic_dump_end   = ORIGIN(PANDUMP) + LENGTH(PANDUMP);

SECTIONS {
    /* ### Boot ROM info */
    /* Goes after .vector_table, to keep it in the first 512 bytes of flash */
    .boot_info : ALIGN(4) {
        KEEP(*(.boot_info));
    } > FLASH
} INSERT AFTER .vector_table;

/* Move .text to start after the boot info */
_stext = ADDR(.boot_info) + SIZEOF(.boot_info);

SECTIONS {
    /* Ubicar la segunda etapa del bootloader al principio de la flash */
    .boot2 : {
        KEEP(*(.boot2));
    } > BOOT2

    /* ### Picotool 'Binary Info' Entries */
    .bi_entries : ALIGN(4) {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    .flash_end : {
        __flash_binary_end = .;
    } > FLASH
} INSERT AFTER .uninit;
