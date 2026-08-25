#!/usr/bin/env python3
"""Rebuild the repository-authored Office 97-2003 acceptance corpus."""
from __future__ import annotations
import argparse, base64, hashlib, pathlib, struct, zlib
TEMPLATES = {
    'normal.doc': (
        'c-rlmT}&KR6vxk<{a}~2uq;TCqHO63NDC~`53ogHm(pb`EM-xwXbrH7Z0&xy(1?i-CdSmnnqYh|X-upMK4}`%H+|3-'
        '`(|uR`p`FgGA1Tg;|mXE{NFowV6)INyCue!IpoKknVoyjx%ZxX=FFWNKbI|k_Eq(-lq1K<LaVBi>{-'
        '_sDAi9~M2ft&s;a7fSvBQ>k~^~Gb7;l<*#MvOBH#c{Pz-i~-Cz$Wk#p{nr6~`tj8F=HbL63ulth`KHz^;fgvVsJXQa-'
        'm!TXGgslRajdi>6J{g-Nw04f7+P!1}<Bf=+$_JS(#D5wVeKn-'
        '|I>Z?WRk;io?4*)NC0@Q;B&<LIcP2eCn1e!q$Xazpd2HHUfI1G+}qu?0m1YO`M@jN3s0lLA{;2F>ZdVwDVKp!{>`oSp>1Op%hPJ?'
        'H`AQ%E+FbvLs5fA~R;5qO-'
        '7y~bWv$Bul|I=8>V3nm<^C7!a5kB#Q<{JHc{s!Yynbd4*dd@SJ%0!#{Qm<W1#FBHuCk7+@T3>2Pcmh9e!HM|2rIY^Y`ug>)eUWKt'
        'M8V>Q*A=6c@%7Av&cQYRQWajpmxmDFGL)bRx*+|y%}Y&Q*6Zb7<8PvH$Vz8GAXG_-pwl`FP6rhl4%sLfv^wU%xsaVELRIuiu*-'
        'T+d-'
        '`d$mXX5x#Tp7y40@v!M{Ipnte#RTU%X8fi}PgnGOUnrs0hl7pu7eO?W8HHfiL@Ayq&{h4f@fCC?c~5J{F^S#A&|zT;z^cFoqVf_7'
        '*RT71iou%+2`9vcg~EG%M~^uuiRL+X0`iL%VgXN7y$7>(aO*j`ownJ~vvM7xwLjeQwxQMMoWsSLOYG*}>{X58GM2h!z8=7Zo<~vv'
        'N^O(<GBboQZ%TJdB5w#~oUf;l~~at!0=lSt_oq3&UCz5@G06<pML}5~Ju&0w&F3IEGNZfs#MwxC`)39zi=sd@h0kPtYW4a>wbYH>'
        'SN8&K8%>7KbKQwsjpoQIoJVgL?X8c%VNf+L|`dr(;}`0qX+7GOmrcjHO^VR&OYBDfa$&1W|9mV2$;0@E|BAaXclQIEM2<5iTPbQR'
        'XUqba>Bab4Gl?yIt1#;keB40b<~faF!XiN#N3)TLc~^&aL#$0^4v^ui9V{Pdx2sQwk6HzU^-'
        '_CmwjjByH`Rd0X=K#$1d$h_cypk(S^6^zW7MjO&XJ9dxkan_FDlTQWamGQV-'
        'kJf>LYC%a_c!E?2!%-7PggTLQF^p(dxSx*-u$D7F?pEEY-abmP?k9?m!C^EX2P+%f;VItYN$xvAxmDF9be!kUst7@ruaxi&p&pOV'
        'P_r(7za_wC)5EJ-46?shMHKvX7x3bjbH=Jjb`*+Fftgw1#F&C3r`hQ_sR&L6}Dc5uS@4pg-'
        'ZkdB`iYYr<0^;pDO&7(hNiP!WG|eEf_E7WCrPv;GfF5>{Z}g_@NMU-OzhaKX4iUU{S-'
        'dpiCBA{mj?)Pr^$ZjFhZU+_y3y)cTJGMzbk$Y|4lG=?c|kjVZ(E3xFRlKmsvS6MY^M_6z21NM73JnnH$Q3F=lbYly#F3rxyJYBZ8'
        '_IoBJDeazR}{!de^<%byL=5JHP+&anAjQuvr}o|F>5UAOW$BnWivJ()s*1gD|UHqm|?ivq3q(7Qn*DSz-'
        'l`Uoo_Ey_P)mb9?ojhBg{SwlrC=4JA^4KHvYit>4LQcN;~JyCuX%GM{_CpFh3!#bo`<z}=(~`t@&yM)mVOs4wO5n9voaK9p0!l9='
        '#Aqf%W!|MvrZ-^sQ?bAIE|S%Cf*!Q3{k;rsdi`!CDC+7J'
    ),
    'normal.ppt': (
        'c-rk-dvH`&8UN0`cOOYMxtj+N0bxf|h_I!Bgx3@#3E?%CB-4;i?PEwK2}F6skP59Jp-'
        '^FpA{`hswIi0IGt!QZ=qUCfsqzPoamsY0Ql>Di*0CVdZm=q#uzue;=kA`pd+%m937OXB?wtEN=X~dUzw^D$xo1zmn*Y`_KOOrv*p'
        '53vhnvwX@G9>Zrt#K85I~dfZbqX~>veSG_(&Z$<dw#79FCs{6OFkKlOIzSrff_(m;#t`G38-0g$O`CrUFcbn2Ios!ZaGw7)-'
        '^OK7nbhlo3jwl=maYhvT>#HsG%lLa+$d<Fx}GflQ9kC^0I%bn4tpvs}8zNc#hC-_lD;hE4zJon8M&boc-3U^t?BwveIVH@7!`x9-'
        '*auU~?r@T<?fOSXM^L#Tekrk0NS4Q=Z?Lvv@_Wr>L}U=YW@%o6E4R`xvkm+COd*$29g&kG;!=xD)K8#lIe_+4e0`jFfOic8*l{Lq'
        'zab!&ppJ?V$3lYjO`8NRzLWt;LVrGHy0WqgH{^Cl_lYo)*2EdAV05X@e@0Wd+{mq|L-'
        'k<R@R*#==we3fD{WMbTF3c9KEJe?yQ^i6nIKj)!#FY2Wd7keT;^%$-'
        '^B4j39(Z7Kicfct#!2A1}X~bxr@s&|U_@!CHaczTj&;lFL%hcj^6JE*htb>Qp=cIFZgF#8F3O!UC)>}JZdGiKCNwh-Hh&^e8RoJU'
        '$>|qF&Lqj^WkZ(b&wqaMxp8Ff>2CfH$>k*S;sxD}iqg?=E9DihHOS#n`%TLnuteMBiT1Rgzz<BIaA;5l<IsiNf*4S5awa_`O_d)2'
        '1pejm6w}!B8VN%^cvJgvMC%L`T8nu>LI}yD}PhIClu-6HCx(v5ECgd1Ti`X=BKwugnz0-'
        '>YcEu897Q<4+xVoZ$mJifrS$dubEJD;)K~98h)piEQqp*WZ_z4!oIJk%5gFnjrNRMeq*Ix+}`hudqpYrJ4zcFN<2oY1vq5h#4L6Y'
        '(868dx6_gO~mMy)bNz-HS_xUR1j7LeOY{_=)if#*gEn%nhxjLpxXUHjm_(dhHs1pu_ee{{>lw)^AH+)U<Bb3S7p&lPPP#dIpg)v1'
        'k%wl1@Hm+zEabGs6`01j*EwP^IXLzk(GYL|(H1@ML`P&>yhQ!d;FbxH5$BWn?Jjxr*a6vUub@;pi&lX-'
        'X8>U&ev_c_#Oz>Io%i>z%Miil8<zN8Cjx~h-'
        'QCn<WL6hT;Z2cO*p5no8y9U+QcT_U5m8_LUjp|>{*$BrF~mBW&;vUG`diyL}#@Zdp(UN0}x+FA}ovb?)oh&j@X_O>RqTV7tyujkL'
        '7=Y3edd^udYbjh`UM1%T*J%pZ~9@lb|2d$!i(CyA|d3O}LqrIFS@e8iDmZf29!!9<ErLeVO7hB6x*xLTuak8~x7hB6x*jlSjFr^3'
        'DizUydKqZvIGyK2>mGLP&(2Fjp6sKtCH5XKhQh1<?E~r>_LMc#b<}qVjR<sx@R%91&?X9S*qT*a9xjhw?blXu;ajv7HBI>B9*z1f'
        '$D^7Ll5tR$lr$o1cqo$wbXo&erNal#L&Xy#xtmaCW_)jdWyV51T6U!R1th2>PEZdX@m!NPN&Vb8s23&?S;4+*6m!fRDjZ4#}iDVz'
        '*V+jdkY1%ZfNNT!OcD%x+6WutMBEq$}bfO#QQp~t5E=`+87fFpyEZg<5;L?PEaneVnwa808@^t=)U&eJ_gWl_rTApu2#EdyB5_E{'
        'cG|1-'
        'lDB$yeEYE1QT)bz7Sav96IT4Crwjm`CMn)xNkHU6N;W2*R!OuJS`Eh>U#bdY=NZ7#n2k`kvMavO*g;8&Q!+!tN{qQ;u@&3fmQ4BS'
        '+<h%q+^Vavme71MY+nNdaviHBu@h8kLawCuOr$e!MJ;3+P#k4MiGiI7Ae+fsp6KBjJoG~}!n@;|`hR>fn`SV(N-'
        'vXO?ZY!4V;Pd`Yq;uFLOiLu~qkN8A5{q65uccZXFU<w7cNDxr8=UciLE}}U6JGb_*zvN~M+mQ{SR5}V@v3I&;k5xC=6JQi`V^SuL'
        'Ib|1Rf_dUA%ydp#9DXZ`74;#V#yUqxeaUfS8g$HyMy<nzmjd(oBqo+;M>h<l@s^t`z=edG6m0*II5#KzG7xPipLCM-X_kY<s3`M)'
        '8Pbpnxg2R?jlb|ruVOZgO{gY1m-i{_gt=%{`sUlZBg{kkn%+I2#IIRNwL)f)=r!X-'
        '(;<YPUg7eNn3YS(cz<Jo~$W6SQgLvA%AP|9Ey#)41rjgf|%igSo4hGL2Qfv-'
        'k>vLSrW0wQX*CwM{IL8=RFl>*VDkPK(?dDFzfLO+`<ZO9yd#0K0LVnF8^C&oN@C@+`g6(Zdyv*&JI}CY9Wppd0JY8-'
        'w!#FSu{K6O1zy`NaV`fepZ24AK>Q`tN?4$vXCDPoupxb&ak~;nrj*+#tfA{>S3R?h9p<w0tU!f;HsA*8W9T+H$K0J1@?I}hz0!dS'
        'l|VPTjqE1ZRNue3xKv8@(bR~agq>^l#qZTA({gRnhOU<DuzSGw*}s0F9#coojCBO;9#2S4_r8Co;e&G&{5#8XHFbsOAfk*%pY7El'
        'x<7!2`;FW%o-'
        'llc4c=LIHTqpkWbJC?MK~lQ>D%0rtxXurVSdmo5dt;wPREH(xH>K>q91OUN<W<dw5VIX>&%+e+#7T?4WR4s#s_8Rpd=~pf@c`@}n'
        '1CUW0yg@1Z@4AB6$>(f9N`PyAa09l1f}>lPFJM+>9}A^tH$<KNe*--'
        '+L6r~vukGF|xKQk}ju3m;4?g?d2v;9e~Q9~_AL;8t}$j_*TST(-'
        'Kvm_t&vZzV&iI`tDmkM4K!`+?dp(ys*?^@izu1wgU?gtU`v<x1H)?l2!O;q!5tA-'
        '3b}iN3Hm)ouNJ2ns_Xm}pcQbBr0rOrzQ$>02!K=~>c8Yal}U#F*rPwYu-IJ=^n19;aAwP)r^@u_y8hiFt(fBn53P3GL=vjJB2r+D'
        'z_hB(yj4`^4Ei=>YAMTg_*w8~LoY6|tv%41dEjy|b*NkZAVh>=@2OCto5NQjpA(r3G36iPk(te@V_sZmloSLmT_<F1(LXn@<$p%^'
        '7`x??PC~fA#oV0n_<@ls$zrc>dp~QJ=0oc#@TkTS7aBr&<y@5o-Fpyv*djkWK$|+fJ4Oj`RFJlI}`Y-'
        'AB^HvLumLAejiW5wVs6lZc4kRP_}pdsx(7EP&hkxFka<|3=TG*Ng9onb#+*5Swa^ew{;|?0)K?#hNPe-qSmbxK*@Sr!F=?t9U>YR'
        '+0AMRH{}nD6C>xCRQ;Rw~EyH!vSd(gMv{V^|XoW>B(EFr^y<c8)<j&skEm!aV=~u>fa-'
        'B9gTFbMDA3!A{5Kk<vG~0JXto;UeTrx3kNL;hmyI&jYE?I4#e3a?{x3&-'
        'cs*muf@05fp4#i<?+V&W=@tTIAVD|%9bahQ(S1RYfz@1!lGPe!lJyx==-FwC?tzok+3LRSOylQC~i?|u-'
        '=uZv?xV_fgPwx`?x0U%t(_eIjB|qT+Po5`FT+lofq?SBR}6$C3a{HUh{T3Xddxi%lFPdggW)tmhkHDJwf_c5gXG){j*{!>-'
        '(=^dnSDx4{8)ecF2f5<r~RyJ&lWdTu%}G0Uq5C#roJ~rmT;-'
        '%F#?NN8gchw2#p~8^tpf3ppyyM2?E%a&)g^ky){jqlL(OF`wsYvGuUm`Q|Fycxt<$py0gYDp!uAzfZOJlK!(uf1Zv0KxK;7seT>2'
        'r`Rd(45V16QpcQ!5W_I$03BmOyl=w){uu|sI|gNb0?PEK0WRQVbrMYi;GbegBewKXuCSA1!|`2~Oi1|1@u582_Rn*xf94Noo}M-'
        '?xc}RJm>RzJW7_BJvAughx<aPRo~%9cJ<4uJYj61qng4*a2tT*ll3s0P{j!>sOB$Ee@Z5m)X6aqaYCqSY(5~Xv7vQT<1JxKMRG}T'
        '13$vgCZpRr~Io>bCyXp9=ff;yLg?Ec!G2WHoZx*JxFdOr#&|=MkIry|y_-'
        '=Lg=EvX|%Kv}y@wfel#HkLAY^82_*oFL`0PPs(d;z|gasLZYBDC1PvGo5}H8<wr>-a8(fgag%S63$gxnR~_6TWss-'
        '`+dqS8)0t!bZIBNW1;9*uJ%AJ>B*RuduwY=Q|TRc`sV{>P6=G59a@8^*H*QILCC?CQ+!t`&HPdbrKi%qZHe>W-'
        '#ftPk6P4t?ye6ydBzKL9)LL+f#R3wDI|6>h?3`pJa7Bv)Z@wnpuD544{3#(*FMd4W_LQ'
    ),
    'normal.xls': (
        'c-'
        'rk(Uuaup6hGh1pWb9$bK9*tI~(Kt({630;)4}e8{17CSUTp^A_dnb>8wqcl6By~YUbRV17(6Q0~vG=qPVG`4+^&U;)61o>_MM8Uq'
        'lp_i6VnF*WWq!yX{>_Ac<q(%n9fFzMOl{_nmwGf8U*JUOx3v+eIpZhsj3;J48VbO#w~!k40o?TEVt$mu%PJKLsm#W*hopzW|VJ-'
        'k|Lo(sLbdPS`^k{N^Y|JLv%CEFGq5;1+m{cfU$_3KhOrDb&xe{d~^foX35@;`rxW&+#7yt^+m#*8`h?5vK%DGq44?0k{#^Dn3DUD'
        '{vF=HeefYGw^nOc2v)dcVfLo|G&GQYi>A%Ojt1(pwHKT!zLHoUxY`T^)HEE`~)1MS@w;g@pK|~qhL^E7*{OABAm1zD@7+kZK5+m{'
        'X)im@jgJDVtv}9M~%41eq6>aGBy$Oe<nOX3gv$4rLQ@9UVT#sfu$Zr{+pt4N+TaWjeIyo31r74rSN~4o&rysmxtrU!*d;<8rMFJ`'
        'WtD1WAgFTfmC)p9rZ`(G{@y(z*x`BSWj|U!f%28vSXf5UyL!GcNF@cmC=nV>GZ#&FF<#t^xWmVpD(6wEko}vL*G`09x6!>xbm1~='
        '>B5*X@~yy8vTTUmvUtDY~_41H|K&fx8(dXZ^$*syfGKB_em%nLtupiPU0BKWM>Z@d;^C)gWr-'
        'efkb<cEBck5<ARajBWDHkTo_m;_X6R?2WVZML;6d6j7din3Xr*|;uzx;y=*xp$V4eJQDV%$5w%xQXbfG(kiHx-uzP=ECTSx`OgiU'
        'xidVvH1+H1GRnG=jI42WY*JQ#*Y?_4VoI9H301Mk)eed%5;fdV?&kH@brggf<5KXHic(UZ*CLCQdz9#Rw74oRiUYGG+@L&3!;pc%'
        '4&$<)q?>pwtcg$ZJ*y>rILPiTgDpr@m4&AC-ZE6f2UD~%i;LZx!%K*>3d^V}kP(*)elQYT4HQzKWckN^)6%m>$qOC|5;HoSrwOyg'
        'oD|yB3Ck$#=uZT7cXl;6si2o_+2R{DF0AEZvv@~o}fiOiCMGYE+{`e&Ru2$jujdrvC^G`Us&zss!*nJ}8%lAqz;?H*qJ(Raf^g9N'
        'Hg^zjIS06E2=+mH6G>Eo~E{WQCzqO`_VLyCqzv<mbxjj~RwSOwuzOd7SS*D}cMd#6D-17`iC9;`=naR1>&X*FYbnKxAd-je`q|)h'
        '>IN0FS<YeE!>HU;z%o?F!lW4odzO;Po-'
        '9N63ykNcmmPuV5pZvhOepyEbKkw81VO;CuN!=%o>RvIa`^8!9alfn(MfJK@^SMyTx*GW<Azb+O!pZwKTPNOu{M}bR+`%=sh&HDCR'
        '5$%t9$CxLZzi~&_dn57Q4Eyg;V6rTob|IfJxkLxA#zMtEq@EFV(7bzUB$g>dxPueLpXN^J~{#!UaaYSpXUx%2~Rt4*J_-NpubDiY'
        '`-whulD|ThrX*x4~nk#5War~eJ0y}7rbCnY{HZ|^q(0}%0s6<KTw7KxpDen_52+;At5rP!~?67r>}<ZzY925zyAyT12HNrHv'
    ),
}
EXPECTED = {
    "normal.doc": (9216, "09e5b5be7573ee5f5ef62c6ef93998f9ec62e7c7297c20212c773771545557da"),
    "normal.ppt": (19968, "16e53bdba2b33533c34f63a710b44fb7fda6cb479855315921b0b07aa1c961d6"),
    "normal.xls": (5632, "e8dc53091dedbe680eae00cd3d812bf8931803512d4e8ecb942e9f19a8b2002d"),
}

PNG_1X1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
)


def add_ppt_picture(contents: bytes) -> bytes:
    """Fill the template's empty Pictures stream with one bounded project-owned PNG."""
    data = bytearray(contents)
    if len(data) % 512 != 0:
        raise RuntimeError("PPT template is not sector aligned")
    sector_count = (len(data) - 512) // 512
    if sector_count + 8 > 128:
        raise RuntimeError("PPT template FAT has no room for the picture stream")
    fat_sector = struct.unpack_from("<I", data, 76)[0]
    fat_offset = (fat_sector + 1) * 512
    encoded_name = "Pictures".encode("utf-16le")
    entries = [
        offset
        for offset in range(0, len(data) - len(encoded_name) + 1, 128)
        if data[offset : offset + len(encoded_name)] == encoded_name
    ]
    if len(entries) != 1:
        raise RuntimeError("PPT template must contain exactly one Pictures directory entry")
    entry = entries[0]
    if struct.unpack_from("<Q", data, entry + 120)[0] != 0:
        raise RuntimeError("PPT template Pictures stream is not empty")
    stream = PNG_1X1 + bytes(4096 - len(PNG_1X1))
    data.extend(stream)
    for index in range(8):
        next_sector = 0xFFFFFFFE if index == 7 else sector_count + index + 1
        struct.pack_into("<I", data, fat_offset + (sector_count + index) * 4, next_sector)
    struct.pack_into("<I", data, entry + 116, sector_count)
    struct.pack_into("<Q", data, entry + 120, len(stream))
    return bytes(data)


def rebuild(output: pathlib.Path, check: bool) -> None:
    output.mkdir(parents=True, exist_ok=True)
    for name, encoded in TEMPLATES.items():
        contents = zlib.decompress(base64.b85decode(encoded))
        if name == "normal.ppt":
            contents = add_ppt_picture(contents)
        observed = (len(contents), hashlib.sha256(contents).hexdigest())
        if observed != EXPECTED[name]:
            raise RuntimeError(f"template authority drifted for {name}: {observed}")
        destination = output / name
        if check:
            if not destination.is_file() or destination.read_bytes() != contents:
                raise RuntimeError(f"fixture differs from generator: {destination}")
        else:
            destination.write_bytes(contents)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path(__file__).parent)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    rebuild(args.output, args.check)


if __name__ == "__main__":
    main()
