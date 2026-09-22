param(
    [Parameter(Mandatory = $true)]
    [string]$SourceRoot
)

$ErrorActionPreference = "Stop"

function Replace-Once {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Old,
        [Parameter(Mandatory = $true)][string]$New,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $text = [System.IO.File]::ReadAllText($Path).Replace("`r`n", "`n")
    $oldNorm = $Old.Replace("`r`n", "`n")
    $newNorm = $New.Replace("`r`n", "`n")

    $first = $text.IndexOf($oldNorm, [System.StringComparison]::Ordinal)
    if ($first -lt 0) {
        throw "Diagnostic patch anchor not found: $Label"
    }
    $second = $text.IndexOf($oldNorm, $first + $oldNorm.Length, [System.StringComparison]::Ordinal)
    if ($second -ge 0) {
        throw "Diagnostic patch anchor is not unique: $Label"
    }

    $patched = $text.Substring(0, $first) + $newNorm + $text.Substring($first + $oldNorm.Length)
    [System.IO.File]::WriteAllText($Path, $patched, [System.Text.UTF8Encoding]::new($false))
}

$cliraop = Join-Path $SourceRoot "src/cliraop.c"
$raop = Join-Path $SourceRoot "src/raop_client.c"

Replace-Once -Path $raop -Label "isolated startup FLUSH probe" -Old @'
	if (!rtspcl_record(p->rtspcl, p->seq_number + 1, NTP2TS(raopcl_get_ntp(NULL), p->sample_rate), kd)) goto erexit;

	if (kd_lookup(kd, "Audio-Latency")) {
'@ -New @'
	{
		uint16_t sairplay_start_seq = p->seq_number + 1;
		uint32_t sairplay_start_ts = NTP2TS(raopcl_get_ntp(NULL), p->sample_rate);

		if (!rtspcl_record(p->rtspcl, sairplay_start_seq, sairplay_start_ts, kd)) goto erexit;

		if (getenv("SAIRPLAY_STARTUP_FLUSH") && *getenv("SAIRPLAY_STARTUP_FLUSH")) {
			LOG_INFO("[SAIRPLAY-DIAG] startup_flush seq=%u rtptime=%u",
					 sairplay_start_seq, sairplay_start_ts);
			if (!rtspcl_flush(p->rtspcl, sairplay_start_seq, sairplay_start_ts)) goto erexit;
		}
	}

	if (kd_lookup(kd, "Audio-Latency")) {
'@

Replace-Once -Path $cliraop -Label "cliraop first accepted/read PCM" -Old @'
		if (status == PLAYING && raopcl_accept_frames(raopcl)) {
			n = read(infile, buf, DEFAULT_FRAMES_PER_CHUNK * 4);
			if (!n)	continue;
			raopcl_send_chunk(raopcl, buf, n / 4, &playtime);
			frames += n / 4;
		}
'@ -New @'
		if (status == PLAYING && raopcl_accept_frames(raopcl)) {
			static bool sairplay_diag_accept_logged = false;
			static bool sairplay_diag_pcm_logged = false;

			if (!sairplay_diag_accept_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] accept_frames=true");
				sairplay_diag_accept_logged = true;
			}

			n = read(infile, buf, DEFAULT_FRAMES_PER_CHUNK * 4);
			if (n > 0 && !sairplay_diag_pcm_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] stdin_pcm_read bytes=%d frames=%d", n, n / 4);
				sairplay_diag_pcm_logged = true;
			}
			if (!n)	continue;
			raopcl_send_chunk(raopcl, buf, n / 4, &playtime);
			frames += n / 4;
		}
'@

Replace-Once -Path $raop -Label "SETUP receiver ports" -Old @'
	if (!p->rtp_ports.audio.rport || !p->rtp_ports.ctrl.rport) {
		LOG_ERROR("[%p]: missing a RTP port in response", p);
		rc = false;
	} else if (!p->rtp_ports.time.rport) {
		LOG_INFO("[%p]: missing timing port, will get it later", p);
	}

	return rc;
'@ -New @'
	if (!p->rtp_ports.audio.rport || !p->rtp_ports.ctrl.rport) {
		LOG_ERROR("[%p]: missing a RTP port in response", p);
		rc = false;
	} else if (!p->rtp_ports.time.rport) {
		LOG_INFO("[%p]: missing timing port, will get it later", p);
	}

	LOG_INFO("[SAIRPLAY-DIAG] setup_ports audio_server=%u control=%u timing=%u",
			 p->rtp_ports.audio.rport, p->rtp_ports.ctrl.rport, p->rtp_ports.time.rport);

	return rc;
'@

Replace-Once -Path $raop -Label "first send_chunk" -Old @'
	pthread_mutex_lock(&p->mutex);

	/*
	 Move to streaming state only when really flushed. In most cases, this is
'@ -New @'
	pthread_mutex_lock(&p->mutex);

	{
		static bool sairplay_diag_send_chunk_logged = false;
		if (!sairplay_diag_send_chunk_logged) {
			LOG_INFO("[SAIRPLAY-DIAG] send_chunk_first frames=%d state=%d audio_fd=%d audio_server=%u",
					 frames, p->state, p->rtp_ports.audio.fd, p->rtp_ports.audio.rport);
			sairplay_diag_send_chunk_logged = true;
		}
	}

	/*
	 Move to streaming state only when really flushed. In most cases, this is
'@

Replace-Once -Path $raop -Label "first UDP send outcome" -Old @'
	if (FD_ISSET(p->rtp_ports.audio.fd, &wfds)) {
		n = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));
		if (n != size) {
			LOG_DEBUG("[%p]: error sending audio packet", p);
			ret = false;
			p->sane.audio.send++;
		}
		else p->sane.audio.send = 0;
		p->sane.audio.avail = 0;
	}
	else {
		LOG_DEBUG("[%p]: audio socket unavailable", p);
		ret = false;
		p->sane.audio.avail++;
	}
'@ -New @'
	{
		static bool sairplay_diag_sendto_logged = false;

		if (FD_ISSET(p->rtp_ports.audio.fd, &wfds)) {
			n = sendto(p->rtp_ports.audio.fd, (void*) packet, + size, 0, (void*) &addr, sizeof(addr));
			if (!sairplay_diag_sendto_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] udp_sendto_first bytes=%d expected=%d audio_server=%u",
						 (int) n, size, p->rtp_ports.audio.rport);
				sairplay_diag_sendto_logged = true;
			}
			if (n != size) {
				LOG_DEBUG("[%p]: error sending audio packet", p);
				ret = false;
				p->sane.audio.send++;
			}
			else p->sane.audio.send = 0;
			p->sane.audio.avail = 0;
		}
		else {
			if (!sairplay_diag_sendto_logged) {
				LOG_INFO("[SAIRPLAY-DIAG] udp_sendto_first skipped=socket_unavailable audio_server=%u",
						 p->rtp_ports.audio.rport);
				sairplay_diag_sendto_logged = true;
			}
			LOG_DEBUG("[%p]: audio socket unavailable", p);
			ret = false;
			p->sane.audio.avail++;
		}
	}
'@

Write-Host "Applied SAirplay2 one-shot libraop runtime diagnostics."
