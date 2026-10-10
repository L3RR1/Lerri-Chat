//! Hora local para as mensagens, sem trazer uma biblioteca de fuso horário.

/// Diferença do horário local para UTC, em segundos.
pub fn utc_offset_secs() -> i64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        // Valor documentado de TIME_ZONE_ID_DAYLIGHT.
        const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
        // SAFETY: estrutura zerada é válida; a API só escreve nela.
        let mut tzi: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        let id = unsafe { GetTimeZoneInformation(&mut tzi) };
        let mut bias = i64::from(tzi.Bias);
        if id == TIME_ZONE_ID_DAYLIGHT {
            bias += i64::from(tzi.DaylightBias);
        } else {
            bias += i64::from(tzi.StandardBias);
        }
        -bias * 60
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// "HH:MM" de um instante Unix no fuso dado.
pub fn hh_mm(unix: u64, offset: i64) -> String {
    let local = (unix as i64 + offset).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, (local % 3600) / 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_hours() {
        assert_eq!(super::hh_mm(0, 0), "00:00");
        assert_eq!(super::hh_mm(3600 * 15 + 60 * 7, -3 * 3600), "12:07");
        assert_eq!(super::hh_mm(60, -3600), "23:01");
    }
}
