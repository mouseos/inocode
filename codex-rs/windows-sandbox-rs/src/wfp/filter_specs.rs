use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMP;
use windows_sys::Win32::Networking::WinSock::IPPROTO_ICMPV6;
use windows_sys::core::GUID;

#[derive(Clone, Copy)]
pub(super) enum ConditionSpec {
    User,
    Protocol(u8),
    RemotePort(u16),
}

#[derive(Clone, Copy)]
pub(super) struct FilterSpec {
    pub(super) key: GUID,
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
}

pub(super) const FILTER_SPECS: &[FilterSpec] = &[
    FilterSpec {
        key: GUID::from_u128(0x38c23ff4_5362_46d8_915f_7f429f6c4562),
        name: "inocode_wfp_icmp_connect_v4",
        description: "Block sandbox-account ICMP connect v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(0x094d671c_bf87_4837_928a_c16d7feae0ea),
        name: "inocode_wfp_icmp_connect_v6",
        description: "Block sandbox-account ICMP connect v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(0x6af16cc8_4b51_4cb5_a741_0c333788468a),
        name: "inocode_wfp_icmp_assign_v4",
        description: "Block sandbox-account ICMP resource assignment v4",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    FilterSpec {
        key: GUID::from_u128(0xcaefb691_4672_4efc_bfdb_218edb21dc17),
        name: "inocode_wfp_icmp_assign_v6",
        description: "Block sandbox-account ICMP resource assignment v6",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    // NAME_RESOLUTION_CACHE filters are intentionally omitted because ordinary
    // static filter shapes returned FWP_E_OUT_OF_BOUNDS during validation.
    FilterSpec {
        key: GUID::from_u128(0x0ab41ea5_1fe7_49fe_8208_20cae8e4585e),
        name: "inocode_wfp_dns_53_v4",
        description: "Block sandbox-account DNS TCP or UDP port 53 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        key: GUID::from_u128(0xca31dbb8_1bb1_4b7e_aaba_ae2bdbff42b4),
        name: "inocode_wfp_dns_53_v6",
        description: "Block sandbox-account DNS TCP or UDP port 53 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    FilterSpec {
        key: GUID::from_u128(0x7ee53248_811a_4ae2_97cb_4ba5eb3b0217),
        name: "inocode_wfp_dns_853_v4",
        description: "Block sandbox-account DNS-over-TLS port 853 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        key: GUID::from_u128(0x3703e41a_e182_4317_a3e0_d8c7dad3d715),
        name: "inocode_wfp_dns_853_v6",
        description: "Block sandbox-account DNS-over-TLS port 853 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    FilterSpec {
        key: GUID::from_u128(0x0092fa38_ec92_4317_a435_22a551fda2f6),
        name: "inocode_wfp_smb_445_v4",
        description: "Block sandbox-account SMB port 445 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        key: GUID::from_u128(0x1cfdf5cc_7b43_4518_abc5_be56954b028d),
        name: "inocode_wfp_smb_445_v6",
        description: "Block sandbox-account SMB port 445 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    FilterSpec {
        key: GUID::from_u128(0x5c5aa808_66de_46fa_aa3a_3907204c64c4),
        name: "inocode_wfp_smb_139_v4",
        description: "Block sandbox-account SMB port 139 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
    FilterSpec {
        key: GUID::from_u128(0xe258669b_ca0c_4061_8b62_8da52322eb46),
        name: "inocode_wfp_smb_139_v6",
        description: "Block sandbox-account SMB port 139 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
];
