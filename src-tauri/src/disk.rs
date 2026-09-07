//! Espaco livre em disco, direto pela API do Windows.
//!
//! Evita uma crate extra so para isso. Se a pasta ainda nao existe (caso comum:
//! o usuario escolheu um destino que sera criado), sobe na arvore ate achar um
//! diretorio existente, que fica no mesmo volume.

use std::path::PathBuf;

fn existing_ancestor(path: &str) -> Option<PathBuf> {
    let mut p = PathBuf::from(path);
    loop {
        if p.is_dir() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
        if p.as_os_str().is_empty() {
            return None;
        }
    }
}

#[cfg(windows)]
pub fn free_bytes(path: &str) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let dir = existing_ancestor(path)?;
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut available: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Some(available)
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn free_bytes(_path: &str) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestral_existente() {
        let tmp = std::env::temp_dir();
        let fundo = tmp.join("nao_existe_1").join("nao_existe_2");
        let achado = existing_ancestor(&fundo.to_string_lossy()).unwrap();
        assert!(achado.is_dir());
    }

    #[test]
    fn espaco_livre_em_pasta_temporaria() {
        let tmp = std::env::temp_dir();
        let livre = free_bytes(&tmp.to_string_lossy());
        assert!(livre.is_some(), "deveria ler o espaco livre");
        assert!(livre.unwrap() > 0);
    }

    #[test]
    fn espaco_livre_em_pasta_inexistente_usa_ancestral() {
        let alvo = std::env::temp_dir().join("tkdrama_nao_existe").join("sub");
        assert!(free_bytes(&alvo.to_string_lossy()).is_some());
    }
}
